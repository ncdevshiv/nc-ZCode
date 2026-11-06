import type { TuiSubmitPromptResult } from "@zcode/tui";
import type {
  DynamicWorkflowRunResumeErrorReason,
  DynamicWorkflowRunSessionSummary,
} from "@zcode/contracts";
import type { CommandCenterDeps } from "../types.js";
import { splitArgs } from "../utils.js";

const DWF_USAGE = "Usage: /dwf [list|cancel [runId]|resume <runId>]";

/** The default number of items in `/dwf list`; the server itself has an upper limit, here it only expresses "one screen is enough". */
const DWF_LIST_LIMIT = 20;

/**
* The state collection of non-final state run. The candidate for `/dwf cancel` when runId is missing is this episode:
* Pending means "already built but not started", and running means "flying". Both of them still have things that can be stopped;
* completed / errored / stopped has been settled, cancellation is meaningless.
 */
const IN_FLIGHT_STATUSES: readonly DynamicWorkflowRunSessionSummary["status"][] = [
  "pending",
  "running",
];

export async function handleDwfCommand(
  args: string,
  deps: CommandCenterDeps,
): Promise<TuiSubmitPromptResult> {
  const app = await deps.getApp();
  const [action = "list", runId] = splitArgs(args);

  if (action === "list") {
    if (!app.listDynamicWorkflowRuns) return unavailable(deps);
    const runs = await app.listDynamicWorkflowRuns({ limit: DWF_LIST_LIMIT });
    return respond(deps, formatRunList(runs));
  }

  if (action === "cancel") {
    return await handleCancel(runId, app, deps);
  }

  if (action === "resume") {
    if (!runId) return respond(deps, DWF_USAGE);
    if (!app.resumeWorkflowRun) return unavailable(deps);
    const result = await app.resumeWorkflowRun({ workId: runId });
    return respond(
      deps,
      result.ok
        ? `Resumed dynamic workflow run ${result.runId}.`
        : `Cannot resume ${runId}: ${describeResumeRejection(result.reason)} (${result.reason})${result.message === undefined ? "" : `\n${result.message}`}`,
    );
  }

  return respond(deps, DWF_USAGE);
}

async function handleCancel(
  runId: string | undefined,
  app: Awaited<ReturnType<CommandCenterDeps["getApp"]>>,
  deps: CommandCenterDeps,
): Promise<TuiSubmitPromptResult> {
  if (!app.cancelBackgroundTask) return unavailable(deps);

  let targetRunId = runId;
  if (!targetRunId) {
// When runId is missing, it will only be determined for the user when "exactly one is flying". Multiple candidates are listed for users to name:
// Canceling costs money and lost progress, and the cost of wrong guessing is asymmetric.
    if (!app.listDynamicWorkflowRuns) return respond(deps, DWF_USAGE);
    const runs = await app.listDynamicWorkflowRuns({ limit: DWF_LIST_LIMIT });
    const inFlight = runs.filter((run) => IN_FLIGHT_STATUSES.includes(run.status));
    if (inFlight.length === 0) {
      return respond(deps, "No in-flight dynamic workflow runs to cancel.");
    }
    if (inFlight.length > 1) {
      return respond(
        deps,
        [
          "Multiple in-flight dynamic workflow runs; pass the run id to cancel one:",
          ...inFlight.map((run) => `- ${formatRunLine(run)}`),
          "",
          "Usage: /dwf cancel <runId>",
        ].join("\n"),
      );
    }
    targetRunId = inFlight[0]!.runId;
  }

// runId ≡ taskId: workflow run is registered with runId in the background task registry.
  const result = await app.cancelBackgroundTask(targetRunId);
  if (!result.cancelled) {
    const reason = result.reason ? `: ${result.reason}` : ".";
    return respond(deps, `Could not cancel ${targetRunId} (${result.status})${reason}`);
  }
  return respond(deps, `Cancelled dynamic workflow run ${targetRunId}.`);
}

function formatRunList(runs: DynamicWorkflowRunSessionSummary[]): string {
  if (runs.length === 0) {
    return "No dynamic workflow runs in this session.";
  }
  return [
    `Dynamic workflow runs (${runs.length}):`,
    ...runs.map((run) => `- ${formatRunLine(run)}`),
    "",
    "Use /dwf cancel <runId> or /dwf resume <runId>.",
  ].join("\n");
}

/**
* One line = `runId · label · status · resumable · updated` + failure suffix.
 *
* `label` and `updatedAt` are additive optional (the old server does not send these two keys): the label falls back to runId,
* The entire time period is omitted. Missing a column is a degeneracy, not a mistake - never hide an entire row because of it. When label is exactly equal to runId
* No duplicate printing: The last file on the server side is runId. If you copy it, you will get "dwfrun_x dwfrun_x".
 */
function formatRunLine(run: DynamicWorkflowRunSessionSummary): string {
  const columns = [run.runId];
  const label = run.label ?? run.runId;
  if (label !== run.runId) columns.push(label);
// stopped with reason word: `stopped/provider`.
  columns.push(run.stopReason === undefined ? run.status : `${run.status}/${run.stopReason}`);
// resumable directly prints the server's ruling, without re-derivating it according to status + failureCode:
//The two predicates will be inconsistent one day, and then the prompt will say that recovery is possible but the command will be rejected.
  if (run.resumable) columns.push("resumable");
  if (run.updatedAt !== undefined) columns.push(`updated ${formatUpdatedAt(run.updatedAt)}`);
  return `${columns.join(" · ")}${formatFailure(run)}`;
}

/**
* epoch milliseconds → readable time. Use the ISO-style short form of the local time zone: the end user sees the time on their own machine,
* The `Z` suffix of UTC needs to be converted in the mind every time when checking locally.
 */
function formatUpdatedAt(updatedAt: number): string {
  const date = new Date(updatedAt);
  if (Number.isNaN(date.getTime())) return String(updatedAt);
  const pad = (value: number): string => String(value).padStart(2, "0");
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${pad(date.getHours())}:${pad(date.getMinutes())}`;
}

function formatFailure(run: DynamicWorkflowRunSessionSummary): string {
  if (!run.failureCode && !run.failureMessage) return "";
  const code = run.failureCode ?? run.status;
  return run.failureMessage ? ` (${code}: ${run.failureMessage})` : ` (${code})`;
}

function describeResumeRejection(reason: DynamicWorkflowRunResumeErrorReason): string {
  switch (reason) {
    case "not_found":
      return "no such run in this session's journal";
    case "not_resumable":
      return "only a stopped run can be resumed (an errored run needs a corrected script via AmendWorkflow)";
    case "superseded":
      return "the run was stopped and superseded by an amended run; that successor is the live one";
    case "already_running":
      return "the run is already in flight";
    case "script_missing":
      return "the recorded run has no script to re-run";
    case "script_mismatch":
      return "the recorded script no longer matches its hash";
    case "compile_failed":
      return "the recorded script no longer compiles against the current workflow facade; rewrite it and use AmendWorkflow";
  }
}

function unavailable(deps: CommandCenterDeps): TuiSubmitPromptResult {
  return respond(deps, "Dynamic workflow runs are not available in this client.");
}

function respond(deps: CommandCenterDeps, response: string): TuiSubmitPromptResult {
  return {
    mode: deps.getMode?.(),
    response,
  };
}
