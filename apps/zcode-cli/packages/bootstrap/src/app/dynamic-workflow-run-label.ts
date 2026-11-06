// ============================================================
// Display label of workflow run (derived when reading, never written back to dwf_run.name)
// ============================================================
//
//Two reasons for independent modules:
// 1. **Two reading surfaces share the same chain**. List (ListWorkflowRuns) and details (GetWorkflowRun) are spelled once each
// If you don't know the bottom line, it will drift on a certain branch, and the symptom is that the same run displays different names in two places.
// 2. **Can be directly tested individually**. run service drags a whole dependency chain of AgentRuntime/engine/journal;
// A pure function shouldn't have to pay the price to be asserted.

import type { DynamicWorkflowRunSummary } from "@zcode/contracts";

/**
* Maximum character limit for script-derived tags. 80 is the length of the list that can be read in one line; the derived value is a heuristic, and the longer it is, the less it looks like a label.
 */
const DYNAMIC_WORKFLOW_RUN_LABEL_MAX_CHARS = 80;

/**
* Derive the display tag of run: `name` → the first non-blank line of the script (truncated to 80 after trimming) → runId.
 *
* Deliberately not doing "smarter" extraction (first comment, regular title search, agent name extraction): the first line is **honest**
* ("The script starts with this"), and each additional heuristic is another way to give misleading labels on other people's scripts.
 *
* Derived results **Never write back** `dwf_run.name`: Logging is equivalent to solidifying a display heuristic into data, and then connecting
* I can’t even answer “Does this run have a name?”
 */
export function resolveDynamicWorkflowRunLabel(input: {
  runId: string;
  name?: string;
  scriptText?: string;
}): Pick<DynamicWorkflowRunSummary, "label" | "labelSource"> {
// A completely blank name is equivalent to no name: it is a line of unclickable blank space in the list, not a label.
  const name = input.name?.trim();
  if (name !== undefined && name.length > 0) return { label: name, labelSource: "name" };

  const firstLine = firstNonEmptyLine(input.scriptText);
  if (firstLine !== undefined) {
    return { label: boundLabel(firstLine), labelSource: "script" };
  }

// Theoretically it won't happen (submit must bring scriptText). labelSource still reports "script" - the only thing the tool needs to distinguish is
// There are two types of "name given by the user" and "named by us". Adding a source value to a branch that should not occur will only make the consumer
// Write one more judgment that will never hit.
  return { label: input.runId, labelSource: "script" };
}

/** The first non-blank line (trimmed). Returns undefined if the entire paragraph is blank. */
function firstNonEmptyLine(scriptText: string | undefined): string | undefined {
  if (scriptText === undefined) return undefined;
  for (const line of scriptText.split("\n")) {
    const trimmed = line.trim();
    if (trimmed.length > 0) return trimmed;
  }
  return undefined;
}

/**
* Cut to the upper limit and never leave a lone surrogate. The script is an external input written by the model, a string literal
* can contain emoji; half of the surrogate pair is neither legal text nor will the downstream JSON codec report an error at some runtimes.
* (Same processing as create-workflow's boundGraphText and port load bounding).
 */
function boundLabel(value: string): string {
  if (value.length <= DYNAMIC_WORKFLOW_RUN_LABEL_MAX_CHARS) return value;
  const cut = value.slice(0, DYNAMIC_WORKFLOW_RUN_LABEL_MAX_CHARS);
  const lastCode = cut.charCodeAt(cut.length - 1);
  const isHighSurrogate = lastCode >= 0xd800 && lastCode <= 0xdbff;
  return isHighSurrogate ? cut.slice(0, -1) : cut;
}
