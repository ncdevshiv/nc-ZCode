// Tool description for AmendWorkflow.
// Written separately from CreateWorkflow: The large section of the model on facade and writing rules has been read on CreateWorkflow.
// Here we only talk about a few things that are unique to the revision - when to use it, how to hit the cache, what things will be used if you omit them, and when the confirmation window appears.

export const AMEND_WORKFLOW_TOOL_DESCRIPTION = [
  "Amend an existing dynamic-workflow run with a revised script or revised settings. Starts a NEW run that supersedes the old one and imports its finished work as a cache, so only what you changed is paid for again. Works on ANY run of this project: completed, errored, stopped — or still running.",
  "",
  "When to use:",
  "- The run errored (the script itself failed): fix the script and amend — every step that already succeeded is imported instead of being paid for twice. Never rewrite the workflow from scratch with CreateWorkflow.",
  "- The run completed but needs one more stage or a refinement: amend — added or changed steps run live, untouched ones cost nothing.",
  "- The run is STILL RUNNING and the user (or a reported item) shows the script is going wrong: amend it NOW, in one call. Do not TaskStop it first and do not wait for it to finish: this tool stops the running predecessor, waits for it to settle, imports everything that settled before the stop, and starts the revision. The earlier you amend, the less is re-paid.",
  "- The user wants the same workflow with fewer subagents at once, its subagents on another model, or another name: amend with only that field and neither `path` nor `script` (see below). On a running run this still stops it, and what it had in flight starts again in the new run.",
  "- To continue a stopped run unchanged, use ResumeWorkflowRun instead. Amending is for when the script or a setting changes.",
  "",
  "How the cache works:",
  "- Named subagents are matched by name across the two scripts; each one's asks are matched in order by byte-identical instructions. A match settles from the recorded result at zero tokens; a changed or added ask runs live. The cache stays open until a live subagent makes its first write to the workspace (or a live `world.run` executes): from that moment, cached world reads and cached asks whose subagent had read or run something would describe a workspace that no longer exists, so they run live too. Asks that only answered keep settling from the cache. Editing an ask's text is how you force it to run again; keep names stable and keep tunable constants out of ask text.",
  "",
// The three bullets refer to the same rule (if omitted, the predecessor will be used), they are deliberately arranged side by side, with the script at the front: if the model thinks that the script is required,
// In order to change a number, the script of thousands of tokens will be copied again; if the two settings are separated, the model will "omit" one of them.
// Read as release. The complete semantics are in the field description (contracts/tools/amend-workflow.ts).
  "What carries over — every field you omit keeps the predecessor's value:",
  "- The script: omit both `path` and `script` to keep the predecessor's script byte for byte and change only the settings below; its finished work still replays from the cache.",
  "- A parallelism limit: omit `max_concurrency` and the new run keeps the predecessor's, pass `null` to remove it, pass a number to change it (only when the user asks).",
  "- The subagents' model: omit `subagent_model` and the new run keeps the predecessor's, pass `null` to put the subagents back on the session model, pass a model id to change it (only when the user asks). ListModels lists the ids.",
  "",
// There are two ways to change the script, normality comes first: the entire script is streamed again
// One pass is the price to be saved, and both the final state notification and GetWorkflowRun have already given the path of the file to the model.
  "Passing a revised script — one of the two, never both:",
  "- `path` is the usual way: the errored notification and GetWorkflowRun name the run's script file — edit it in place and pass the same path back, so a revision costs one Edit instead of a second copy of the whole script. A file whose bytes you did not change is refused (`script_unchanged`) unless the call also sets `max_concurrency` or `subagent_model`; nothing is stopped and nothing is created.",
  "- `script` is still accepted for a revision you write from scratch: the WHOLE revised script, inline, written against the same facade and rules as CreateWorkflow (phases, subagent names, gates, `return` shape). It is saved to a draft file and the result names it, so the next revision can go back to `path`. Compilation errors come back as diagnostics: fix the file the result names and call again with `path` — nothing was stopped or started.",
  "",
  "Confirmation:",
  "- A run this session started (through CreateWorkflow, a previous AmendWorkflow, or the workflows hub) is amended without a confirmation window, even while it is running. A run the user stopped, or a run another session started, asks the user first.",
  "- The result names the run that was superseded (if one was stopped) and the new run's ID. The superseded run sends no notification of its own; the new run notifies when it settles. Do not wait for it or poll it with TaskOutput.",
].join("\n");
