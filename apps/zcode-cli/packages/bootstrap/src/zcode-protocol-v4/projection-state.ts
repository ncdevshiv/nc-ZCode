// ProductProjection state factory and derivation rules.
// This file only contains pure functions: initial snapshot, availability/inputRouting derivation, revision progressive determination.
import type {
  ActionAvailability,
  ConversationDelta,
  ConversationSnapshot,
  GoalState,
  InputRouting,
  SessionActionAvailability,
  SessionControl,
  StatePatch,
} from "@zcode/shared/zcode-protocol-v4";

/**
* TraceId sentinel for cold recovery synthetic events.
* Synthetic events are "view reconstruction" rather than authoritative user actions: ModelSelected and other config class synthetic events
* Update projection config, but must not declare seed authority (configModelTouchedByEvent) -
* Otherwise, the last round of models in history will overwhelm the true runtime value of resume writeback (including draft selection).
 */
export const HYDRATION_TRACE_ID = "hydrate-trace";

export function createInitialConversationSnapshot(
  sessionId: string,
  logEpoch: string,
): ConversationSnapshot {
  return {
    protocolVersion: 1,
    sessionId,
    logEpoch,
    seq: 0,
    revision: 0,
    control: {
// draft ruling: Session entity exists but no input has been made yet.
      phase: "draft",
      sessionEnded: false,
      canStop: false,
      stopState: "idle",
      stopTargetKind: "unknown",
      activeWorks: [],
      lastError: null,
      apiRetry: null,
    },
    availability: computeAvailability({
      phase: "draft",
      goalStatus: null,
      compacting: false,
      goalVerifying: false,
      queueLength: 0,
      autoDrain: true,
    }),
    inputRouting: computeInputRouting(
      {
        phase: "draft",
        goalStatus: null,
        compacting: false,
        goalVerifying: false,
        queueLength: 0,
        autoDrain: true,
      },
      "queue",
    ),
    meta: { title: "", titleSource: "default" },
// Mode initial value = core default collaboration mode ("build" fallback of session-mode-port getMode).
// Although the SessionCreated event has mode, the draft semantics require no visible delta (no bump revision);
// When the persistence preference is not build, the first SessionModeChanged shall prevail (drafts carry no revision seed channel by design).
    config: {
      provider: "",
      model: "",
      thought: "",
      thoughtLevels: [],
      followupMode: "queue",
      mode: "build",
    },
    modelTransition: null,
    usage: {
      contextWindow: null,
      cumulative: {
        inputTokens: 0,
        outputTokens: 0,
        cacheReadTokens: 0,
        cacheWriteTokens: 0,
      },
    },
    queue: { items: [], autoDrain: true },
    pendingInteractions: [],
    pendingCommands: [],
    backgroundWorks: [],
    subagents: { revision: 0, childSessionIds: [], running: [], endedTotal: 0 },
    goal: null,
    plan: null,
//Soft access control: initial no pending review status; after activated() reports, it will be written by projection.
    workspaceHookAdmission: null,
    rows: { window: [], totalCount: 0, firstRowId: null },
  };
}

const ALLOWED: ActionAvailability = { allowed: true };

function denied(reasonCode: string): ActionAvailability {
  return { allowed: false, reasonCode };
}

// guard-derived input face (same origin as projection). compacting/goalVerifying is not a separate phase
// (phase closed enumeration), passed in after being derived from activeWorks/goal.status.
// queueLength/autoDrain for held derivation.
interface AvailabilityContext {
  phase: SessionControl["phase"];
  goalStatus: GoalState["status"] | null;
  compacting: boolean;
  goalVerifying: boolean;
  queueLength: number;
  autoDrain: boolean;
}

//The verdict table is aligned with the evaluate of packages/formal-proof/src/model.ts one by one
// (gold test formal-proof-consistency endorsement); reasonCode = product-protocol guard id.
export function computeAvailability(context: AvailabilityContext): SessionActionAvailability {
  const { phase, goalStatus, compacting } = context;
  const running = phase === "running" || phase === "prewarming";
  const compact = compacting
    ? // formal-proof: duplicateCompactRejected —— running/queued compact 操作锁去重。
      denied("compactOperationLock")
    : phase === "draft"
      ? // formal-proof: idleCannotCompact —— 没有可压缩上下文。
        denied("idleCannotCompact")
      : // running/goal verifier 时命令进入 typed FIFO，completed 时立即执行或进入 held queue。
        ALLOWED;
  return {
    fork: compacting
      ? denied("compactOperationLock")
      : phase === "draft"
        ? denied("forkTargetNotStable")
        : ALLOWED,
    compact,
    switchModelConfig: ALLOWED,
    setFollowupMode: ALLOWED,
    queueEdit: ALLOWED,
    sendQueuedNow: compacting
      ? denied("compactOperationLock")
      : running
        ? ALLOWED
        : denied("sendQueuedNowRequiresRunning"),
// Independent pauseGoal changes the target product state; during the verifier/notSatisfied period, the underlying target is still active.
// Therefore, suspension is also allowed, and the provider abort controller cannot be required to currently exist.
    pauseGoal:
      goalStatus === "active" || goalStatus === "verifying" || goalStatus === "notSatisfied"
        ? ALLOWED
        : denied(goalStatus === null ? "noGoalToPause" : "goalNotActive"),
// The reverse operation of resumeGoal = stopPausesActiveGoalTarget: only paused can be resumed.
    resumeGoal:
      goalStatus === "paused"
        ? ALLOWED
        : denied(goalStatus === null ? "noGoalToResume" : "goalNotPaused"),
  };
}

export function computeInputRouting(
  context: AvailabilityContext,
  followupMode: "queue" | "guide",
): InputRouting {
  // formal-proof: compactingAcceptsFutureInput
// —— compact is a maintenance step, and the input is the future intention → join the queue without interrupting compact.
  if (context.compacting) {
    return { mode: "enqueue", reasonCode: "compactingAcceptsFutureInput" };
  }
// goal verifier is completion-blocking active work, but not normal
// assistant active turn; only look at phase=running will try steer in guide mode,
// Core does not have steerable activeTurn at this time, causing user input to neither enter queue nor history.
  if (context.goalVerifying) {
    return { mode: "enqueue", reasonCode: "goalVerifierAcceptsFutureInput" };
  }
  if (context.phase === "running" || context.phase === "prewarming") {
    return { mode: followupMode === "guide" ? "guide" : "enqueue" };
  }
// When completed + queue>0 + autoDrain=false, the input will not be queued silently;
// The client presents the clear/keep selection, and the disposition goes up with the command.
  const completed =
    context.phase === "completedSuccess" || context.phase === "completedInterrupted";
  if (completed && context.queueLength > 0 && !context.autoDrain) {
    return { mode: "choice", reasonCode: "heldQueueInputRequiresChoice" };
  }
  return { mode: "startNow" };
}

// revision progression rule (closed definition): row structure changes + changes in area A (except usage/pendingCommands) +1 each.
// `workflowRuns` **intentionally not listed**, falls under the exemption class of usage/pendingCommands: it is high-frequency derived data
// (about 5 migrations per node), comes with monotonic revision field, and does not have any command
// baseRevision CAS read it - inclusion will cause run to jitter on every node migration during conversation revision,
// Failed to increment CAS falsely.
const REVISION_BEARING_PATCH_KEYS: ReadonlyArray<keyof StatePatch> = [
  "control",
  "availability",
  "inputRouting",
  "meta",
  "config",
  "queue",
  "pendingInteractions",
  "backgroundWorks",
  "subagents",
  "goal",
  "plan",
];

export function deltaBumpsRevision(delta: ConversationDelta): boolean {
  switch (delta.op) {
    case "row.appended":
    case "row.upserted":
    case "row.removed":
      return true;
    case "row.delta":
      return false;
    case "state.updated":
      return REVISION_BEARING_PATCH_KEYS.some((key) => delta.patch[key] !== undefined);
  }
}
