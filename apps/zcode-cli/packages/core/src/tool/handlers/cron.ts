// ============================================================
// Cron Tool Handlers
// ============================================================

import {
  CoreErrorType,
  CronCreateInputJsonSchema,
  CronCreateInputSchema,
  CronCreateOutputJsonSchema,
  CronCreateOutputSchema,
  CronDeleteInputJsonSchema,
  CronDeleteInputSchema,
  CronDeleteOutputJsonSchema,
  CronDeleteOutputSchema,
  CronListInputJsonSchema,
  CronListInputSchema,
  CronListOutputJsonSchema,
  CronListOutputSchema,
  CronUpdateInputJsonSchema,
  CronUpdateInputSchema,
  CronUpdateOutputJsonSchema,
  CronUpdateOutputSchema,
  createCoreError,
  type CronAutomation,
  type CronCreateInput,
  type CronCreateOutput,
  type CronDeleteInput,
  type CronDeleteOutput,
  type CronListOutput,
  type CronUpdateInput,
  type CronUpdateOutput,
  type ToolPermissionSpec,
} from "@zcode/contracts";
import type { ToolEntry, ToolExecutionContext, ToolHandler } from "../types.js";

const CRON_TOOL_TIMEOUT_MS = 30_000;
const CRON_MODEL_BYTES = 32_000;

function assertNotAutomationTurn(
  context: ToolExecutionContext,
  toolName: "CronCreate" | "CronUpdate" | "CronDelete",
): void {
  if (!context.automationTurn) return;
// provider tool denylist is only a visibility constraint, old entries or exceptions may still be submitted directly by the provider
// automation writing tool. The handler must make the final rejection based on the current round facts passed in by the executor, and cannot call the port.
  throw createCoreError(
    CoreErrorType.PermissionDenied,
    `${toolName} is not allowed while running a scheduled automation.`,
    {
      context: {
        toolCallId: context.toolCallId,
        toolName,
      },
      recoverable: false,
      retryable: false,
    },
  );
}

function assertAutomationPort(
  context: ToolExecutionContext,
  toolName: "CronCreate" | "CronList" | "CronUpdate" | "CronDelete",
): asserts context is ToolExecutionContext & {
  automationPort: NonNullable<ToolExecutionContext["automationPort"]>;
} {
  if (context.automationPort) return;
  throw createCoreError(
    CoreErrorType.ConfigurationError,
    `AutomationPort is not configured for ${toolName}`,
    {
      context: {
        toolCallId: context.toolCallId,
        toolName,
      },
      recoverable: false,
    },
  );
}

function toModelAutomation(automation: CronAutomation): CronAutomation {
  return {
    automationId: automation.automationId,
    title: automation.title,
    cronExpr: automation.cronExpr,
    prompt: automation.prompt,
    enabled: automation.enabled,
    lifecycleStatus: automation.lifecycleStatus,
    nextRunAt: automation.nextRunAt,
    lastRunAt: automation.lastRunAt,
    runCount: automation.runCount,
    recurring: automation.recurring,
    maxRuns: automation.maxRuns,
// Tool output used to miss scheduleRule when reprojecting automation here, causing CronCreate /
// CronUpdate / CronList only displays compatible cron even if real intervals are received, errors are displayed as hourly or daily.
    scheduleRule: automation.scheduleRule,
  };
}

const cronCreateHandler: ToolHandler = async (input, context) => {
  assertNotAutomationTurn(context, "CronCreate");
  const parsed = CronCreateInputSchema.parse(input) as CronCreateInput;
  assertAutomationPort(context, "CronCreate");

  const automation = await context.automationPort.create(parsed, {
// When creating a scheduled task in a session, the model comes from the current runtime, not the tool input parameters that are controllable by the model.
    ...(context.model ? { model: `${context.model.providerId}/${context.model.modelId}` } : {}),
// The cron created within the session will always reuse the current session, and subsequent triggers will no longer create new sessions.
    sessionId: context.sessionId,
  });
  return {
    automation: toModelAutomation(automation),
    message: `Created automation ${automation.automationId}.`,
  } satisfies CronCreateOutput;
};

const cronListHandler: ToolHandler = async (input, context) => {
  CronListInputSchema.parse(input);
  assertAutomationPort(context, "CronList");

  const automations = await context.automationPort.list();
  return {
    automations: automations.map(toModelAutomation),
  } satisfies CronListOutput;
};

const cronUpdateHandler: ToolHandler = async (input, context) => {
  assertNotAutomationTurn(context, "CronUpdate");
  const parsed = CronUpdateInputSchema.parse(input) as CronUpdateInput;
  assertAutomationPort(context, "CronUpdate");

  const automation = await context.automationPort.update(parsed);
  return {
    automation: toModelAutomation(automation),
    message: `Updated automation ${automation.automationId}.`,
  } satisfies CronUpdateOutput;
};

const cronDeleteHandler: ToolHandler = async (input, context) => {
  assertNotAutomationTurn(context, "CronDelete");
  const parsed = CronDeleteInputSchema.parse(input) as CronDeleteInput;
  assertAutomationPort(context, "CronDelete");

  const deleted = await context.automationPort.delete(parsed);
  return {
    deleted,
    id: parsed.id,
    message: deleted
      ? `Deleted automation ${parsed.id}.`
      : `Automation ${parsed.id} was not found in the current workspace.`,
  } satisfies CronDeleteOutput;
};

function cronPermission(
  permission: string,
  reason: string,
  needsApproval: boolean,
): ToolPermissionSpec {
  return {
    permission,
    reason,
    riskLevel: "medium" as const,
    sideEffectScope: "workspace" as const,
    needsApproval,
    patternSources: ["toolName", "input"],
    alwaysAllowPatternSources: needsApproval ? undefined : ["toolName"],
    denyPriority: "beforeAsk" as const,
  };
}

const cronResultBudget = {
  maxInlineBytes: CRON_MODEL_BYTES,
  maxModelBytes: CRON_MODEL_BYTES,
  strategy: "truncate" as const,
  preview: {
    maxBytes: CRON_MODEL_BYTES,
    direction: "head" as const,
  },
};

const cronTimeout = {
  defaultMs: CRON_TOOL_TIMEOUT_MS,
  maxMs: CRON_TOOL_TIMEOUT_MS,
  allowCallOverride: false,
};

export const cronCreateToolEntry: ToolEntry = {
  capability: "Create a scheduled automation for the current workspace",
  metadata: {
    name: "CronCreate",
// Second-order scheduling requirements should directly constrain the model in the tool contract; natural language cannot be reliably determined by keywords or regularity.
//The mutation tool denylist of the automation execution wheel is the permission boundary that prevents recursive modification of task definitions.
    description:
      "Create a persistent scheduled automation in the current workspace. It uses the host's real current clock for relative delayMinutes schedules, or a standard 5-field cron expression in the user's local timezone for absolute/recurring schedules, and survives app restarts. The prompt must describe the final scheduled work directly and must never ask the run to create, schedule, or configure another automation or call CronCreate.",
    modelInstructions: [
      "Use this only when the user explicitly asks to schedule future automatic work.",
      "Interpret cron in the user's local timezone using fields: minute hour day-of-month month day-of-week. Do not convert to UTC.",
// 'Reminder for class in 8 minutes' is both a relative delay and a one-time reminder; in the old command, "a one-time reminder means pin
//Absolute month, day, hour and minute" wording covers the relative delay rule, and the model self-calculates a fixed calendar cron like '29 7 29 7 *' accordingly.
// The model is often stale to the "now" time, and the self-calculated one-time time is silently rolled to the next year by the host once it has just passed.
// Fix: Any expression of "N days from now" (including hours, Chinese and English) will all use delayMinutes; pin absolute cron only
// Used for wall clock dates explicitly called by the user, and explicitly declared to be relatively one-off, delayMinutes must be used instead.
      "For any schedule expressed as a delay from now — 'in 3 minutes' sets delayMinutes=3, 'in 2 hours' sets delayMinutes=8, 'in 2 hours' sets delayMinutes=120, 'later'/'later' — set delayMinutes to the total whole minutes, omit cron, set recurring=false, and omit maxRuns. The host anchors to its real current clock; never infer the current time or convert a relative delay into a cron or clock time yourself.",
      "Use '*/20 * * * *' for every 20 minutes, '0 * * * *' for hourly, and '0 9 * * 1-5' for weekdays at 09:00.",
// Only using cron to express "every N units" will be affected by the field upper limit, and will become wall clock aligned even if N does not cross the boundary.
// Inconsistent with the semantics of UI custom repeat anchoring from save moment. All N units are unified with carrier + scheduleRule.
      "For every N minutes/hours/days/weeks/months/years, always set intervalUnit (minute|hourly|daily|weekly|monthly|yearly) and interval together. interval must be an integer from 1 to 200, including values cron could express directly. Supply a legal 5-field compatible cron only for time-of-day/day/weekday/month slots; never put an out-of-range step in cron. Examples: every 20 minutes -> intervalUnit='minute', interval=20, cron='* * * * *'; every 31 hours at minute 49 -> intervalUnit='hourly', interval=31, cron='49 * * * *'; every 40 days at 09:00 -> intervalUnit='daily', interval=40, cron='0 9 * * *'. Omit intervalUnit/interval only for ordinary calendar cron schedules, such as weekdays at 09:00.",
      "Pin minute, hour, day-of-month, and month in cron only for an absolute wall-clock date the user names outright, such as 'tomorrow at 9am' or 'on July 30 at 20:00'; set recurring=false and omit maxRuns (the default limit is 1). A relative one-shot such as '8 minutes later' or 'in 2 hours' must use delayMinutes instead, because a self-computed one-shot time that has just passed silently rolls a full year forward.",
      "For exactly N scheduled runs, set recurring=false and maxRuns=N. recurring=true is indefinite and must not be combined with maxRuns.",
      "Automations persist in the current workspace until the user deletes them. Finite automations become completed and retain their history; they are not session-only or auto-deleted.",
      "Honor exact user-provided times without adding jitter or shifting the schedule.",
      "Do not include workspace paths or identities in the input; the current session workspace is used.",
      "Always set title and preserve the user's natural-language schedule phrase verbatim in it. The title may be concise, but must not omit timing such as 'every 20 minutes', 'every morning at 9am', or 'every Friday'.",
      "Write prompt as a complete instruction that can run later without relying on unstated conversation context.",
      "Write the final work directly in prompt. Never ask the scheduled run to create, schedule, or configure another automation, and never ask it to call CronCreate.",
    ],
    readOnly: false,
    destructive: false,
    concurrentSafe: false,
    timeoutMs: CRON_TOOL_TIMEOUT_MS,
    maxOutputBytes: CRON_MODEL_BYTES,
    sideEffectScope: "workspace",
    riskLevel: "medium",
    needsApproval: true,
  },
  handler: cronCreateHandler,
  inputSchema: CronCreateInputJsonSchema,
  outputSchema: CronCreateOutputJsonSchema,
  runtimeInputSchema: CronCreateInputSchema,
  runtimeOutputSchema: CronCreateOutputSchema,
  permission: cronPermission(
    "automation.create",
    "CronCreate creates a scheduled background automation for this workspace",
    true,
  ),
  resultBudget: cronResultBudget,
  timeout: cronTimeout,
  cancellation: {
    supported: true,
    cleanup: "none",
    userVisibleMessage: "CronCreate was cancelled before the automation was created",
  },
  trace: {
    required: true,
    propagateToAdapters: true,
    recordInput: "summary",
    recordOutput: "summary",
  },
};

export const cronListToolEntry: ToolEntry = {
  capability: "List scheduled automations for the current workspace",
  metadata: {
    name: "CronList",
    description: "List scheduled automations in the current workspace.",
    readOnly: true,
    destructive: false,
    concurrentSafe: true,
    timeoutMs: CRON_TOOL_TIMEOUT_MS,
    maxOutputBytes: CRON_MODEL_BYTES,
    sideEffectScope: "none",
    riskLevel: "low",
    needsApproval: false,
  },
  handler: cronListHandler,
  inputSchema: CronListInputJsonSchema,
  outputSchema: CronListOutputJsonSchema,
  runtimeInputSchema: CronListInputSchema,
  runtimeOutputSchema: CronListOutputSchema,
  permission: {
    permission: "automation.read",
    reason: "CronList only reads scheduled automations for this workspace",
    riskLevel: "low",
    sideEffectScope: "none",
    needsApproval: false,
    patternSources: ["toolName"],
    alwaysAllowPatternSources: ["toolName"],
    denyPriority: "beforeAsk",
  },
  resultBudget: cronResultBudget,
  timeout: cronTimeout,
  cancellation: {
    supported: true,
    cleanup: "none",
    userVisibleMessage: "CronList was cancelled before automations were returned",
  },
  trace: {
    required: true,
    propagateToAdapters: false,
    recordInput: "summary",
    recordOutput: "summary",
  },
};

export const cronUpdateToolEntry: ToolEntry = {
  capability: "Update a scheduled automation in the current workspace",
  metadata: {
    name: "CronUpdate",
    description:
      "Update selected definition fields of an existing scheduled automation in the current workspace while preserving its id and run history.",
    modelInstructions: [
      "Use CronList first when the automation id is not already known. Never guess an automation id.",
      "Always pass title on every CronUpdate. Rewrite it so it describes the task after the update and keeps the user's natural-language schedule phrase consistent with cron; for example, changing every 5 minutes to every 6 minutes must also update the title.",
      "Apart from the required synchronized title, only pass fields the user asked to change. Omitted fields preserve their existing values.",
      "Interpret cron in the user's local timezone using five fields: minute hour day-of-month month day-of-week. Do not convert to UTC.",
// A carrier must also be used when updating "every N units"; otherwise small intervals will degenerate into wall clock crons, and long intervals will generate illegal crons.
      "To create or change every N minutes/hours/days/weeks/months/years, pass intervalUnit and interval together for every N-unit schedule. interval must be an integer from 1 to 200 even when cron could express N. Also pass a legal compatible cron with only the time/day/weekday/month slot; for example, every 40 days at 09:00 uses intervalUnit='daily', interval=40, cron='0 9 * * *'. Omit the pair only when preserving or using an ordinary calendar cron schedule.",
      "Use numeric maxRuns only with recurring=false. Setting recurring=true clears any old finite limit automatically; never combine recurring=true with a numeric maxRuns.",
      "CronUpdate cannot change workspace, session binding, model, provider, mode, thought level, run count, history, or enabled state.",
      "Do not simulate an update by deleting and recreating the automation.",
      "After a successful update, reply with only a brief confirmation. Do not restate the automation fields in a fenced code block or simulate a text file because the UI renders the updated automation card.",
    ],
    readOnly: false,
    destructive: false,
    concurrentSafe: false,
    timeoutMs: CRON_TOOL_TIMEOUT_MS,
    maxOutputBytes: CRON_MODEL_BYTES,
    sideEffectScope: "workspace",
    riskLevel: "medium",
    needsApproval: true,
  },
  handler: cronUpdateHandler,
  inputSchema: CronUpdateInputJsonSchema,
  outputSchema: CronUpdateOutputJsonSchema,
  runtimeInputSchema: CronUpdateInputSchema,
  runtimeOutputSchema: CronUpdateOutputSchema,
  permission: cronPermission(
    "automation.update",
    "CronUpdate changes a scheduled background automation in this workspace",
    true,
  ),
  resultBudget: cronResultBudget,
  timeout: cronTimeout,
  cancellation: {
    supported: true,
    cleanup: "none",
    userVisibleMessage: "CronUpdate was cancelled before the automation was updated",
  },
  trace: {
    required: true,
    propagateToAdapters: true,
    recordInput: "summary",
    recordOutput: "summary",
  },
};

export const cronDeleteToolEntry: ToolEntry = {
  capability: "Delete a scheduled automation from the current workspace",
  metadata: {
    name: "CronDelete",
    description: "Delete a scheduled automation from the current workspace by automation id.",
    readOnly: false,
    destructive: true,
    concurrentSafe: false,
    timeoutMs: CRON_TOOL_TIMEOUT_MS,
    maxOutputBytes: CRON_MODEL_BYTES,
    sideEffectScope: "workspace",
    riskLevel: "medium",
    needsApproval: true,
  },
  handler: cronDeleteHandler,
  inputSchema: CronDeleteInputJsonSchema,
  outputSchema: CronDeleteOutputJsonSchema,
  runtimeInputSchema: CronDeleteInputSchema,
  runtimeOutputSchema: CronDeleteOutputSchema,
  permission: cronPermission(
    "automation.delete",
    "CronDelete removes a scheduled background automation from this workspace",
    true,
  ),
  resultBudget: cronResultBudget,
  timeout: cronTimeout,
  cancellation: {
    supported: true,
    cleanup: "none",
    userVisibleMessage: "CronDelete was cancelled before the automation was deleted",
  },
  trace: {
    required: true,
    propagateToAdapters: true,
    recordInput: "summary",
    recordOutput: "summary",
  },
};
