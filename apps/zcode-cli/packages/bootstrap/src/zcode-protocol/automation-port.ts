import {
  AutomationCreateLimitError,
  hasRelativeDelayMinutes,
  isAutomationCreateLimitError,
  type AutomationPort,
  type CronAutomation,
} from "@zcode/contracts";
import {
  parseModelPickerValue,
  zcodeAutomationCheckTaskBindingResultSchema,
  zcodeAutomationCreateResultSchema,
  zcodeAutomationDeleteResultSchema,
  zcodeAutomationListResultSchema,
  zcodeAutomationUpdateResultSchema,
  zcodeProtocolMethods,
  type ZCodeAutomationProtocol,
} from "@zcode/shared";
import {
  ProtocolRequestError,
  type ZCodeProtocolAgentServerContext,
  type ZCodeProtocolSessionRecord,
} from "./server-types.js";

const AUTOMATION_CREATE_FROM_AUTOMATION_RUN_ERROR =
  "Cannot create a scheduled task while running a scheduled task.";

const AUTOMATION_CREATE_IN_BOUND_SESSION_ERROR =
  "Cannot create a scheduled task inside a session that already belongs to a scheduled task. " +
  "Ask the user to start a new chat to create another scheduled task.";

const AUTOMATION_CREATE_BOUND_SESSION_CHECK_ERROR =
  "Cannot verify whether this session belongs to a scheduled task. Try again later.";

export function createProtocolAutomationPort(
  context: ZCodeProtocolAgentServerContext,
// Belonging session parser: automation-port is constructed based on session and is directly bound to the session it serves.
// record". Avoid relying on context.sessions lookup - V4/desktop sessions may not be registered in this legacy map,
// Previously, it would cause activeSession hit failure and loss of permissions/thinking levels (the model survived only if createContext was in place).
  resolveOwnSession?: () => ZCodeProtocolSessionRecord | undefined,
): AutomationPort {
  return {
    async create(input, createContext) {
// Priority is given to the belonging session (the session where this tool is running); when returning, press createContext.sessionId to check the legacy map.
      const activeSession =
        resolveOwnSession?.() ??
        (createContext?.sessionId ? context.sessions.get(createContext.sessionId) : undefined);
      if (activeSession?.activeAutomationId?.trim()) {
// The current turn has been dispatched by automationId; continuing CronCreate will form a recursive scheduled task chain.
// This judgment occurs within automation-port, protocol automation/create will not be called when hit.
        throw new Error(AUTOMATION_CREATE_FROM_AUTOMATION_RUN_ERROR);
      }
// Desktop interactive input is directly connected to the CLI (bypassing the toolDenylist injection of the host adapter). Ordinary users can
// When input continues in a session that has been assigned to a scheduled task, CronCreate is still registered and there is no per-turn filtering, which will be bypassed
// All previous guards create scheduled tasks again. Here, when creating the entrance, press targetTaskId to make a secret that has nothing to do with the entrance path:
// As long as the current session is already a binding session of an automation (= scheduled task session), refuse to create it again; ordinary session
// It has not been bound when it is first created, so it is released normally. The ownership judgment must use the dedicated EXISTS protocol, and the complete list cannot be read; otherwise
// Any damage to the display field of any historical task will cause the current session to falsely report "unverifiable".
      const ownSessionId = createContext?.sessionId ?? activeSession?.app.sessionId;
      if (ownSessionId) {
        let bound: boolean;
        try {
          try {
            const result = await context.requestClient(
              zcodeProtocolMethods.automationCheckTaskBinding,
              { targetTaskId: ownSessionId },
              zcodeAutomationCheckTaskBindingResultSchema,
            );
            bound = result.bound;
          } catch (error) {
            if (!(error instanceof ProtocolRequestError && error.code === -32601)) {
              throw error;
            }
// The protocol version is still 1, and the new CLI connects to the old Host that is still alive or remote before the upgrade, and the dedicated home is
//The method will return -32601. Only "the method does not exist" can prove to be a capability difference. In this case, the old version of list filtering will be rolled back;
// Database, transport and protocol errors are still handed over to the outer layer fail-closed and cannot be misjudged as unbound.
            const legacyResult = await context.requestClient(
              zcodeProtocolMethods.automationList,
              {},
              zcodeAutomationListResultSchema,
            );
            bound = legacyResult.automations.some(
              (automation) => automation.targetTaskId === ownSessionId,
            );
          }
        } catch (error) {
          context.logger?.warn("Failed to check bound automations before CronCreate", {
            errorMessage: error instanceof Error ? error.message : String(error),
            event: "automation.create.bound_session_check.failed",
            sessionId: ownSessionId,
          });
// Session ownership queries are authorization boundaries that prevent recursive CronCreate; unknown cannot be equated with unbound,
// Otherwise, the host/database temporary failure will reopen the creation ability. Query failure must be fail-closed.
          throw new Error(AUTOMATION_CREATE_BOUND_SESSION_CHECK_ERROR);
        }
        if (bound) {
          throw new Error(AUTOMATION_CREATE_IN_BOUND_SESSION_ERROR);
        }
      }
// CronCreate's tool context only carries part of the configuration, causing permissions or thinking levels to be lost when crossing layers.
// The protocol boundary reads the active runtime by sessionId, ensuring that the configuration currently seen by the user triggering the tool is saved.
      const runtimeModelSelection =
        activeSession?.app.runtime.getSessionModelSelection() ??
        (() => {
          try {
            return createContext?.model ? parseModelPickerValue(createContext.model) : undefined;
          } catch {
            return undefined;
          }
        })();
      const runtimeMode = activeSession?.app.getMode();
      const hasIntervalCarrier = input.intervalUnit !== undefined && input.interval !== undefined;
      let result;
      try {
        result = await context.requestClient(
          zcodeProtocolMethods.automationCreate,
          {
// Relative time is not converted into absolute time by the model; the compatible protocol still requires cronExpr, and the service layer will overwrite the occupancy value with the real clock.
// `!== null` determines that ordinary cron calls that omit delayMinutes will mistakenly take the relative branch.
// Force recurring=false; the only caliber of relative tasks is hasRelativeDelayMinutes (an explicit number).
            cronExpr: input.cron ?? "* * * * *",
            ...(hasRelativeDelayMinutes(input) ? { relativeDelayMinutes: input.delayMinutes } : {}),
            prompt: input.prompt,
            title: input.title,
            recurring: hasRelativeDelayMinutes(input)
              ? false
              : hasIntervalCarrier
                ? true
                : (input.recurring ?? true),
// workspace is still injected by the protocol server from the current session.
            ...(runtimeModelSelection ? { modelSelection: runtimeModelSelection } : {}),
            ...(runtimeMode ? { mode: runtimeMode === "auto" ? "build" : runtimeMode } : {}),
            ...(createContext?.sessionId ? { targetTaskId: createContext.sessionId } : {}),
            ...(hasIntervalCarrier
              ? {
// The new carrier does not have a limited upper limit of transparent transmission; the create protocol does not have maxRuns=null clearing semantics.
                  intervalUnit: input.intervalUnit,
                  interval: input.interval,
                }
              : input.maxRuns !== undefined
                ? { maxRuns: input.maxRuns }
                : {}),
          },
          zcodeAutomationCreateResultSchema,
        );
      } catch (error) {
        if (!isAutomationCreateLimitError(error)) throw error;
//Protocol errors used to enter the tool loop as ordinary Errors, and the model would treat the deletion suggestions in the errors as
// Can perform recovery steps, then iterate CronList/CronDelete/CronCreate. Mapping into stable domain errors,
// Allow core to close the tool recovery boundary of the current turn without relying on shared implementation.
        throw new AutomationCreateLimitError(
          error instanceof Error ? error.message : String(error),
          error,
        );
      }
      const automation = toCronAutomation(result.automation);
      const title = automation.title.trim();
      if (activeSession && title.length > 0) {
        try {
// In-session CronCreate will create automation in the first round of replies, but the first message triggers
// The session_title sidecar may arrive late and generate the title based on helper interpretation, causing the title to become "I can't...".
// After CronCreate is successful, fix the current session title to the automation title and prevent the generated title from being overwritten.
          await activeSession.app.setCustomSessionTitle({
            title,
            traceContext: activeSession.traceContext,
          });
        } catch (error) {
          context.logger?.warn("Failed to freeze session title after CronCreate", {
            errorMessage: error instanceof Error ? error.message : String(error),
            event: "automation.session_title_freeze.failed",
            sessionId: createContext?.sessionId,
          });
        }
      }
      return automation;
    },
    async update(input) {
      const hasIntervalCarrier = input.intervalUnit !== undefined && input.interval !== undefined;
      const result = await context.requestClient(
        zcodeProtocolMethods.automationUpdate,
        {
          automationId: input.id,
          ...(input.title !== undefined ? { title: input.title } : {}),
          ...(input.cron !== undefined ? { cronExpr: input.cron } : {}),
          ...(input.prompt !== undefined ? { prompt: input.prompt } : {}),
          ...(hasIntervalCarrier
            ? {
//Historical one-time tasks are only transparently transmitted. The carrier will retain recurring=false after the first dispatch.
// Marked completed by the repository. The carrier must atomically switch to an infinite loop at protocol boundaries.
                recurring: true,
                maxRuns: null,
                intervalUnit: input.intervalUnit,
                interval: input.interval,
              }
            : {
                ...(input.recurring !== undefined ? { recurring: input.recurring } : {}),
                ...(input.maxRuns !== undefined ? { maxRuns: input.maxRuns } : {}),
              }),
        },
        zcodeAutomationUpdateResultSchema,
      );
      return toCronAutomation(result.automation);
    },
    async list() {
      const result = await context.requestClient(
        zcodeProtocolMethods.automationList,
        {},
        zcodeAutomationListResultSchema,
      );
      return result.automations.map(toCronAutomation);
    },
    async delete(input) {
      const result = await context.requestClient(
        zcodeProtocolMethods.automationDelete,
        { automationId: input.id },
        zcodeAutomationDeleteResultSchema,
      );
      return result.deleted;
    },
  };
}

function normalizeCronAutomationMode(
  mode: ZCodeAutomationProtocol["mode"],
): CronAutomation["mode"] {
  switch (mode) {
    case undefined:
      return undefined;
    case "plan":
    case "edit":
    case "yolo":
    case "build":
      return mode;
    case "auto":
    case "autoEdit":
      return "build";
    default: {
      const exhaustiveMode: never = mode;
      return exhaustiveMode;
    }
  }
}

function toCronAutomation(input: ZCodeAutomationProtocol): CronAutomation {
  return {
    automationId: input.automationId,
    title: input.title,
    cronExpr: input.cronExpr,
    prompt: input.prompt,
    enabled: input.enabled,
    lifecycleStatus: input.lifecycleStatus,
    nextRunAt: input.nextRunAt,
    lastRunAt: input.lastRunAt,
    runCount: input.runCount,
    recurring: input.recurring,
    maxRuns: input.maxRuns,
    modelSelection: input.modelSelection,
// Explicitly downgrade according to the existing session permission semantics. When a new mode is added to the protocol in the future, exhaustive checking will force synchronization.
    mode: normalizeCronAutomationMode(input.mode),
// Transparently transmit the authoritative scheduleRule; the session card must read this field to display the real interval that cron cannot express.
// (such as every 50 hours, every 40 days), otherwise error displays such as "00th minute of every hour" can only be inferred from compatible cronExpr.
    scheduleRule: input.scheduleRule,
  };
}
