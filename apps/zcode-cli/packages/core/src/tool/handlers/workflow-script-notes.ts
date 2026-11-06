// ============================================================
//Three sentences of the script file on the model side
// ============================================================
//
// The tool response is one of three readers (the other two are final notification and `GetWorkflowRun`), and the three readers want to push the model to the same
// Next step: **Go edit that file**. So just one thing to put here - know which file the script falls after, the diagnostic line with NOTE
//How to write. `CreateWorkflow` and `AmendWorkflow` share it, because "after changing the `path`, hand it back" in both tools
// The above is the same sentence. If written separately, it will branch into two sentences.
//
// When the file is unknown (a project that cannot be written in draft), there is no sentence here: the caller retains the old copy before the change, and the model still reads
// "Change the script and submit it again."

import type { CreateWorkflowDiagnostic } from "@zcode/contracts";

/**
* The identity of the script file in the model. `kind` affects only one verb: `draft` is a copy ("saved at") of what the tool just wrote**,
* `path` is the file given by the model itself ("The script file is") - saying "saved to" a file that already exists,
* Reads like the tool just touched it.
 */
export interface WorkflowScriptLocation {
  kind: "draft" | "path";
/** The writing method of the model surface (workspace relative or absolute), calculated by `describeWorkflowScriptPath`. */
  described: string;
/** Text line → Offset of file line; no metadata block is 0. */
  lineOffset: number;
}

/**
* Diagnostic model interface. If there is a file, write it as `{path}:L{line}:C{column} {message}`, and the line number is according to the number of **file**——
* This number must be able to be directly pasted into an `Edit` of the file. Fall back to the old `L:C` form (text lines) when there is no file.
 *
* The `diagnostics` array and display load in the output are not changed accordingly: the transcription surface shows the text, and the text line is its coordinates.
 */
export function formatWorkflowDiagnosticLines(
  diagnostics: readonly CreateWorkflowDiagnostic[],
  location: WorkflowScriptLocation | undefined,
): string[] {
  if (location === undefined) {
    return diagnostics.map(
      (diagnostic) => `L${diagnostic.line}:C${diagnostic.column} ${diagnostic.message}`,
    );
  }
  return diagnostics.map(
    (diagnostic) =>
      `${location.described}:L${diagnostic.line + location.lineOffset}:C${diagnostic.column} ${diagnostic.message}`,
  );
}

/**
* NOTE when the script cannot be edited and the script has a file (inline and `path` two sources). The last half of the sentence is the purpose of this entire feature:
* **Don’t post the script again**.
 */
export function workflowScriptFileNote(location: WorkflowScriptLocation): string {
  const where =
    location.kind === "draft"
      ? `The script is saved at ${location.described}.`
      : `The script file is ${location.described}.`;
  return `NOTE: The workflow was NOT executed. ${where} Edit that file in place and resubmit with \`path: "${location.described}"\` — do not paste the script inline again.`;
}

/**
* Cannot be edited, the source is NOTE when defined as saved. Let me tell you two more things: This copy is **copied from which definition** (the model knows based on this
* What is changed is the copy, not the definition), and the definition itself needs to be changed through `SaveWorkflow`. The actual parameters need to be passed again - copied with the metadata block,
* The declarations are still there, so `path` commits will also verify them.
 */
export function workflowSavedDraftNote(options: {
  savedName: string;
  savedPath: string;
  draft: string;
}): string {
  return `NOTE: The workflow was NOT executed. A working copy of the saved workflow '${options.savedName}' (${options.savedPath}) was written to ${options.draft}. Edit that copy in place and resubmit with \`path: "${options.draft}"\` (pass its \`args\` again); to change the saved definition itself, use SaveWorkflow.`;
}

/** A sentence appended after successful startup: The next revision starts from editing this file. */
export function workflowLaunchedScriptSentence(location: WorkflowScriptLocation): string {
  const where =
    location.kind === "draft"
      ? `The script is saved at ${location.described}`
      : `The script file is ${location.described}`;
  return ` ${where}; to revise it later, edit that file and pass \`path\` to AmendWorkflow.`;
}

/** A sentence appended after the revision is started successfully (revising again will still be the same action). */
export function workflowAmendedScriptSentence(location: WorkflowScriptLocation): string {
  return ` The revision's script is at ${location.described}; edit it there for a further revision.`;
}
