// ============================================================
// The shared type that stops dispatching in the background
// ============================================================
// Extracted from background.ts for use with each stop branch module dispatched by RuntimeTaskType
// (see background-stop-dynamic-workflow.ts). Separate files are used so that branch modules do not need to import
// background.ts prevents the two from importing types from each other, and also limits the size of background.ts.
//
// background.ts continues to re-export these names, so agent-runtime.ts /
// The existing import path of internal-turn-methods.ts remains unchanged.

import type { BackgroundTaskInfo, BackgroundTaskInfoStatus } from "../deps.js";
import type { RuntimeTaskSnapshot, RuntimeTaskType } from "../../runtime-task/registry.js";
import type { TraceContext } from "../deps.js";

export type RuntimeBackgroundStopFailureReason =
  | "background_task_cancel_not_supported"
  | "background_task_not_found"
  | "background_task_not_running";

export type RuntimeBackgroundStopStatus = BackgroundTaskInfoStatus | RuntimeTaskSnapshot["status"];

export type RuntimeBackgroundStopResult =
  | {
      alreadyTerminal?: boolean;
      command?: string;
      ok: true;
      status: RuntimeBackgroundStopStatus;
      taskId: string;
      type: RuntimeTaskType;
    }
  | {
      reason: RuntimeBackgroundStopFailureReason;
      ok: false;
      status?: RuntimeBackgroundStopStatus;
      taskId: string;
      type?: RuntimeTaskType;
    };

/**
* GUI/backend panel
* `runtime.cancelBackgroundTask` comes in as `"user"`, and the model's `TaskStop` is `"model"`; runtime
* System paths such as cleaning are not filled in. The dwf branch records it on the registry entry, and the final notification tells the model "This is the user's
* Decide not to recover on your own".
 */
export type RuntimeBackgroundStopInitiator = "user" | "model";

export interface RuntimeBackgroundStopOptions {
  initiator?: RuntimeBackgroundStopInitiator;
  strict?: boolean;
  traceContext?: TraceContext;
}

export interface RuntimeBackgroundStopTarget {
  currentStatus: RuntimeBackgroundStopStatus | undefined;
  existing: BackgroundTaskInfo | undefined;
  registryTask: RuntimeTaskSnapshot | undefined;
  taskId: string;
  taskType: RuntimeTaskType | undefined;
}

/** The stop target of taskType has been determined; each branch module only accepts this form. */
export type TypedRuntimeBackgroundStopTarget = RuntimeBackgroundStopTarget & {
  taskType: RuntimeTaskType;
};
