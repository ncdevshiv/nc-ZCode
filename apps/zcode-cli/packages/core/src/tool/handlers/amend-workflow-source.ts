// ============================================================
// AmendWorkflow - which script to run for this revision
// ============================================================
//
// The script has three sources, which are unified into the same set of fields: `path` (normal: a script file modified in place), `script` (the entire inline copy),
// Give neither of them (use the one saved by the predecessor). Split from resolveInput body (amend-workflow-resolve.ts)
// The reason is the same as `create-workflow-source.ts`: This is the place to **read the world** (script files, precursor archived scripts),
// The handler is where **modify the world**. When the two are mixed together, the next person will naturally read the disk again in the handler, and the confirmation window will see
// is no longer the byte that will be executed.

import {
  AMEND_WORKFLOW_SOURCE_ERROR,
  AmendWorkflowInputSchema,
  type AmendWorkflowInput,
  type DynamicWorkflowRunPort,
} from "@zcode/contracts";
import type { ToolHandlerFailure } from "../types.js";
import { readWorkflowScriptFile } from "./workflow-path-source.js";

/**
* Local failure code table. The value is just a log bit (the executor is projected into `code: "N"`), and the discrimination key is in the message prefix;
* `run_not_found` reuses the same key and code of the introspection table ("the referenced run does not exist" is the same thing on the three tools).
* Edited from 21 onwards only for visual non-collision with introspection table (1/2), ResumeWorkflowRun (11–15).
 */
export const AMEND_WORKFLOW_ERROR_CODE = {
  AMEND_UNAVAILABLE: 21,
  MISSING_BOUNDARIES: 22,
  SUBAGENT_MODEL: 23,
  SCRIPT_UNCHANGED: 24,
  SCRIPT_FILE: 25,
  SCRIPT_UNAVAILABLE: 26,
} as const;

/** The code for parameter-level violation (given by both sources) is the same 400 as `CreateWorkflow`. */
const AMEND_WORKFLOW_INPUT_FAILURE_CODE = 400;

/**
* The revised script ** gives at most one **, which is only true for the input parameters sent by the model (the reason is the same as `validateCreateWorkflowSource`: normalization
* After that, if `script` and `path` are present at the same time, it is a legal execution state). Not giving both is not a violation - it is "following the predecessor's script".
 */
export function validateAmendWorkflowSource(input: unknown): { result: true } | ToolHandlerFailure {
  const parsed = AmendWorkflowInputSchema.safeParse(input);
  if (!parsed.success) return { result: true };
  if (parsed.data.script !== undefined && parsed.data.path !== undefined) {
    return {
      result: false,
      errorCode: AMEND_WORKFLOW_INPUT_FAILURE_CODE,
      message: AMEND_WORKFLOW_SOURCE_ERROR,
    };
  }
  return { result: true };
}

/**
* Both sources are omitted, but there is no script to follow. Both reasons have the same key: the next step for the model is the same thing - hand in the script.
 *
* - `record`: There is no script in the predecessor record (the old run before the script was dropped into the library);
* - `host`: This session does not have a run port, or the port does not have `getScript` (old host).
 */
export function scriptUnavailableFailure(
  runId: string,
  cause: "record" | "host",
): ToolHandlerFailure {
  const why =
    cause === "record"
      ? `run ${runId} has no stored script to keep (it predates script persistence)`
      : `this host cannot read run ${runId}'s stored script`;
  return {
    result: false,
    errorCode: AMEND_WORKFLOW_ERROR_CODE.SCRIPT_UNAVAILABLE,
    message: `workflow_amend_script_unavailable: ${why}, so the script cannot be omitted here — pass the whole script as \`script\`, or its file as \`path\`. Nothing was stopped or created.`,
  };
}

/** The normalized script fields (the three sources are of the same shape), plus the file writing method and "whether to inherit" that should be seen on the model interface. */
type AmendScriptResolution =
  | {
      result: true;
      fields: { script: string; path?: string; script_line_offset?: number };
      described?: string;
      inherited: boolean;
    }
  | ToolHandlerFailure;

/**
* The three sources are unified into the same set of fields.
 *
* - `path`: read file. With metadata blocks the blocks are stripped and declarations ignored - revisions take no arguments (the arguments are the predecessor run
* fact, as journal goes), so there is nothing to verify here; the block still has to be stripped, otherwise it would be considered part of the script
* Feed into the compiler.
* - `script`: as is.
* - All omitted: read the copy of the predecessor archive through the port (the same bytes replayed by resume). Read in resolveInput and not in
* handler: The confirmation window must draw a picture of the script to be run, and the hook and project rules must also match it, and both of these are
* before handler. The empty string is synonymous with absence - the runtime schema's `.min(1)` does not collect empty scripts. The script still needs to be used
* Recognize home once ({@link resolveKeptScriptFile}): If the predecessor script file still has this byte, the new run will continue
* Remember it, otherwise `path` is missing and handler writes a new draft according to the rules of "script not from file".
 *
* When `port` is absent (unconnected host), the first two are as usual, but the third cannot be used and fails on the spot.
 */
export async function resolveAmendScript(options: {
  model: AmendWorkflowInput;
  cwd: string;
  port: DynamicWorkflowRunPort | undefined;
/** The script file (absolute path) on the predecessor snapshot; used to identify the home when inheriting the script. */
  predecessorScriptPath: string | undefined;
}): Promise<AmendScriptResolution> {
  const { model, cwd, port } = options;
  if (model.path !== undefined) {
    const read = await readWorkflowScriptFile({ cwd, inputPath: model.path });
    if (!read.ok) {
      return {
        result: false,
        errorCode: AMEND_WORKFLOW_ERROR_CODE.SCRIPT_FILE,
        message: `workflow_script_file_unreadable: ${read.message} Nothing was stopped or created.`,
      };
    }
    return {
      result: true,
      inherited: false,
      described: read.file.described,
      fields: {
        script: read.file.script,
        path: read.file.path,
        ...(read.file.bodyLineOffset === 0 ? {} : { script_line_offset: read.file.bodyLineOffset }),
      },
    };
  }
  if (model.script !== undefined) {
    return { result: true, inherited: false, fields: { script: model.script } };
  }
  if (port === undefined || typeof port.getScript !== "function") {
    return scriptUnavailableFailure(model.run_id, "host");
  }
  const stored = await port.getScript(model.run_id);
  if (stored === undefined || stored.length === 0) {
    return scriptUnavailableFailure(model.run_id, "record");
  }
  const kept = await resolveKeptScriptFile({
    cwd,
    scriptPath: options.predecessorScriptPath,
    script: stored,
  });
  return {
    result: true,
    inherited: true,
    ...(kept === undefined ? {} : { described: kept.described }),
    fields: {
      script: stored,
      ...(kept === undefined
        ? {}
        : {
            path: kept.path,
            ...(kept.lineOffset === 0 ? {} : { script_line_offset: kept.lineOffset }),
          }),
    },
  };
}

/**
* Inherited script recognition: If the script file written down by the predecessor is **at this moment**
* The same bytes are still read out, and the new run will continue to remember this file - the script has not changed, the file has not changed, it is the new run's
* Script file, you will still need to edit it for the next revision of the model. The tool inheritance and GUI "configuration" share this article.
 *
* If there is any mismatch (the file has not been recorded in the predecessor, the file is gone, cannot be read, or has been modified), `undefined` will be returned: the caller's
* Write a new draft of "script not from file". Never record a file whose contents are no longer this script to a new run - that would make
* The diagnostic line number and "Go edit that file" both point to another piece of code.
 */
export async function resolveKeptScriptFile(options: {
  cwd: string;
  scriptPath: string | undefined;
  script: string;
}): Promise<{ path: string; described: string; lineOffset: number } | undefined> {
  if (options.scriptPath === undefined) return undefined;
  const read = await readWorkflowScriptFile({ cwd: options.cwd, inputPath: options.scriptPath });
  if (!read.ok || read.file.script !== options.script) return undefined;
  return {
    path: read.file.path,
    described: read.file.described,
    lineOffset: read.file.bodyLineOffset,
  };
}

/**
* "The file has not been modified" pre-check.
 *
* This is only true for the ** `path` given by the ** model: what this network wants to catch is that ** forgot to edit **, and pasted the script again, or explicitly said "the script does not move"
* (omitted from both sources) is not that error. This is only true if nothing has been changed in this call - and `max_concurrency` is set
* or `subagent_model` revision has its own meaning, it is reasonable for the script to be unchanged (`null` counts as "passed": it is a one-time
* Explicitly released).
 *
* `getScript` is an optional member of the port, and in its absence this precheck is skipped: it is a convenience, not a gate to correctness.
 */
export async function refuseUnchangedScript(options: {
  port: DynamicWorkflowRunPort;
  /** 模型发出的入参（归一化之前）：沿用时回填的 `path` 不算「模型给的」。 */
  model: AmendWorkflowInput;
  resolvedScript: string;
  described: string | undefined;
}): Promise<ToolHandlerFailure | undefined> {
  const { port, model, resolvedScript, described } = options;
  if (model.path === undefined) return undefined;
  if (model.max_concurrency !== undefined || model.subagent_model !== undefined) return undefined;
  if (typeof port.getScript !== "function") return undefined;

  const previous = await port.getScript(model.run_id);
  if (previous === undefined || previous !== resolvedScript) return undefined;
  return {
    result: false,
    errorCode: AMEND_WORKFLOW_ERROR_CODE.SCRIPT_UNCHANGED,
    message: `workflow_script_unchanged: ${described ?? model.path} is byte-for-byte the script run ${model.run_id} was started with, so amending it would repeat that run exactly. Edit the file first, then call AmendWorkflow again with the same \`path\`; to change only the settings, pass them and omit \`path\`; to continue a stopped run under the same script, use ResumeWorkflowRun. Nothing was stopped or created.`,
  };
}
