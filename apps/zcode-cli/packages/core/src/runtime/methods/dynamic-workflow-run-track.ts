import { CREATE_WORKFLOW_TOOL_NAME, type TraceContext } from "@zcode/contracts";
import type { ExecutableToolCall } from "../../tool/types.js";
import type { AgentRuntimeInternal } from "../internal.js";

/**
* Put a **restored** dwf run back into background tracking.
 *
* The tracking of the submit path is automatically completed by the tool executor when CreateWorkflow returns backgrounded; resume
* is a v4 command, there is no tool call on the fly, so four things no one does: runtime-task registry registration (** session
* Recycling guardrail** - the session that missed it is recycled as idle, and the completion notification has nowhere to be delivered), BackgroundTaskStarted
* (backgroundWorks panel + cancelable flag), polling/final state waiter, settlement notification. This method synthesizes a
* The tool descriptor uses the same trackBackgroundTask of the executor, and four things are exchanged at once.
 *
* Describe the trade-offs of subfields:
* - `id` uses the **original toolCallId** (dwf_run.tool_call_id is restored from the library) - it is the tool card → details page
* The associated key is the parentToolCallId of the registry; when the old run is absent, a resume prefix id is synthesized (observable,
* Not pretending to be any real tool shop).
* - `name` is always CreateWorkflow: per-tool life cycle dispatch (snapshot/wait/cancel/notify format) and click it to look up the table.
* - `input.name` feeds the display name link (workflowTaskSubject) - the resume command can be carried, in absence
* The tracker goes to the taskId, and the UI side presses the fallbackName for processing (same semantics as the submit path).
 *
* turnId is deliberately absent: resume is initiated by UI command and does not belong to any model turn (similar to run progress event
* rootTraceContext argument comes from the same source, see dynamic-workflow-run-progress.ts).
 */
export async function trackResumedDynamicWorkflowRun(
  this: AgentRuntimeInternal,
  input: { runId: string; toolCallId?: string; name?: string; traceContext?: TraceContext },
): Promise<void> {
  const traceContext = input.traceContext ?? this.rootTraceContext;
  const toolCall: ExecutableToolCall = {
    id: input.toolCallId ?? `resume-${input.runId}`,
    name: CREATE_WORKFLOW_TOOL_NAME,
    input: input.name === undefined ? {} : { name: input.name },
  };
  await this.executor.trackExternalBackgroundTask(
    toolCall,
    { backgroundTaskId: input.runId, status: "backgrounded" },
    traceContext,
    undefined,
  );
}
