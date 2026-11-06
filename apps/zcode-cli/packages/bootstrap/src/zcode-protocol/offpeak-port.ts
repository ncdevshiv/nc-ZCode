import type { OffPeakPort, OffPeakTaskSummary } from "@zcode/contracts";
import {
  zcodeOffPeakCreateResultSchema,
  zcodeOffPeakListResultSchema,
  zcodeProtocolMethods,
  type ZCodeOffPeakTaskProtocolSnapshot,
} from "@zcode/shared";
import type {
  ZCodeProtocolAgentServerContext,
  ZCodeProtocolSessionRecord,
} from "./server-types.js";

const OFF_PEAK_CREATE_FROM_OFF_PEAK_RUN_ERROR =
  "Cannot create an idle-time task while running an idle-time task.";
const OFF_PEAK_CREATE_IN_BOUND_SESSION_ERROR =
  "This session already has a pending idle-time task. Wait for it to finish (or cancel it in Automations) before creating another one here.";
const OFF_PEAK_CREATE_BOUND_SESSION_CHECK_ERROR =
  "Cannot verify whether this session already has a pending idle-time task; try again later.";

const OFF_PEAK_TERMINAL_STATUSES = new Set(["completed", "failed", "cancelled"]);

/**
* Off-Peak protocol port. Trade-offs with automation-port:
* - Only prevent "free time wheel recursive creation" (activeOffPeakTaskId); do not check activeAutomationId - cron automatic wheel
* Release OffPeakCreate (scheduled idle time task).
* - Create and bind the current session within the session (aligned with CronCreate targetTaskId), and resume execution of this session when dispatched.
* The bound guard only rejects "this session already has an unterminated idle task" (two unattended prompts grab the same session); the task can be re-established after it is terminated.
* Unlike cron's per-session, per-task permanent rejection. Determine reuse offPeak/list minimum snapshot (including sessionId/status),
* Query failed fail-closed. The authority for amplification protection is still the server-side account quota (POST /ticket 3103).
* - Do not inject runtimeModel/mode/thoughtLevel: resolve on host side by default
* (yolo / allowed_models last position / highest inference file), the session running state has nothing to do with the idle whitelist.
* - Do not freeze the session title: it is bound to the user's work session, and the title should not be rewritten by the task.
 */
export function createProtocolOffPeakPort(
  context: ZCodeProtocolAgentServerContext,
  resolveOwnSession?: () => ZCodeProtocolSessionRecord | undefined,
): OffPeakPort {
  return {
    async create(input, createContext) {
      const activeSession =
        resolveOwnSession?.() ??
        (createContext?.sessionId ? context.sessions.get(createContext.sessionId) : undefined);
      if (activeSession?.activeOffPeakTaskId?.trim()) {
// This turn has been dispatched by the idle time task; create another idle time task in the idle time round = recursive self-derivation, directly rejected.
// This judgment is the third layer of depth besides turn denylist and handler offPeakTurn.
        throw new Error(OFF_PEAK_CREATE_FROM_OFF_PEAK_RUN_ERROR);
      }
      const boundSessionId = createContext?.sessionId ?? activeSession?.app.sessionId;
      if (boundSessionId) {
        let bound: boolean;
        try {
          const listed = await context.requestClient(
            zcodeProtocolMethods.offPeakList,
            {},
            zcodeOffPeakListResultSchema,
          );
          bound = listed.tasks.some(
            (task) =>
              task.sessionId === boundSessionId && !OFF_PEAK_TERMINAL_STATUSES.has(task.status),
          );
        } catch (error) {
          context.logger?.warn("Failed to check bound idle-time tasks before OffPeakCreate", {
            errorMessage: error instanceof Error ? error.message : String(error),
            event: "offpeak.create.bound_session_check.failed",
            sessionId: boundSessionId,
          });
// Unknown cannot be equated to unbound: temporary host/database failure cannot reopen "same session dual tasks".
          throw new Error(OFF_PEAK_CREATE_BOUND_SESSION_CHECK_ERROR);
        }
        if (bound) throw new Error(OFF_PEAK_CREATE_IN_BOUND_SESSION_ERROR);
      }
      const result = await context.requestClient(
        zcodeProtocolMethods.offPeakCreate,
        {
          title: input.title,
          prompt: input.prompt,
          ...(input.permissionMode ? { permissionMode: input.permissionMode } : {}),
          ...(input.model ? { model: input.model } : {}),
          ...(input.thoughtLevel ? { thoughtLevel: input.thoughtLevel } : {}),
// Bind the current session; the workspace is still injected from the current session by the host (symmetric automation/create).
          ...(boundSessionId ? { boundSessionId } : {}),
        },
        zcodeOffPeakCreateResultSchema,
      );
      if (!result.ok) {
// The failure classification is transparently transmitted to the handler for translation as it is, and is prohibited from being downgraded to a message string at the port layer.
        return {
          ok: false,
          failureStage: result.failureStage,
          errorCategory: result.errorCategory,
          errorCode: result.errorCode,
        };
      }
      return { ok: true, task: toOffPeakTaskSummary(result.task) };
    },
    async list() {
      const result = await context.requestClient(
        zcodeProtocolMethods.offPeakList,
        {},
        zcodeOffPeakListResultSchema,
      );
      return result.tasks.map(toOffPeakTaskSummary);
    },
  };
}

function toOffPeakTaskSummary(input: ZCodeOffPeakTaskProtocolSnapshot): OffPeakTaskSummary {
  return {
    offPeakTaskId: input.offPeakTaskId,
    title: input.title,
    status: input.status,
    queuePosition: input.queuePosition,
    sessionId: input.sessionId,
    createdAt: input.createdAt,
  };
}
