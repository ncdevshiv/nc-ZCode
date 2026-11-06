// ============================================================
// Cron automation tools - session-level scheduled task management
// ============================================================

import { z } from "zod";
import { toToolJsonSchema } from "./json-schema.js";

const nonEmptyString = z.string().trim().min(1);
// contracts still use Zod 3, while shared protocols use Zod 4; cross-version schema instances are not composable.
// The field contract remains consistent leaf by leaf with the shared ModelSelection, and the local declaration will be deleted after the Zod version is unified later.
const cronModelSelectionSchema = z
  .object({
    providerId: nonEmptyString,
    modelId: nonEmptyString,
    options: z
      .object({
        reasoningLevel: nonEmptyString.optional(),
        maxOutputTokens: z.number().int().positive().optional(),
      })
      .strict()
      .optional(),
  })
  .strict();

/** The only caliber for relative task determination: only explicit positive integers are relative delays.
* delayMinutes used to be required nullable, and ordinary cron creation was forced to pass null explicitly——
* Old callers that omit fields/unstable providers that retain null will fail to parse directly, or in
* `!== null` determines that undefined is mistakenly taken to the relative branch and recurring tasks are mistakenly created as one-time.
* Both undefined and null represent "non-relative" and are backward compatible with the existing { cron, prompt, title } protocol. */
export function hasRelativeDelayMinutes(input: { delayMinutes?: number | null }): boolean {
  return typeof input.delayMinutes === "number";
}

export const CronCreateInputSchema = z
  .object({
    cron: nonEmptyString
      .optional()
      .describe(
        "Standard 5-field cron expression in the user's local timezone: minute hour day-of-month month day-of-week. Use it only for an absolute named date/time or a recurring schedule; required unless delayMinutes is set. For any relative delay such as 'in 8 minutes' or 'in 2 hours', omit cron and use delayMinutes instead — never convert a relative phrase into a clock fixed time or calendar date, because a just-passed one-shot time silently rolls a full year forward. Examples: '*/20 * * * *' means every 20 minutes, '0 * * * *' means hourly, and '0 9 * * 1-5' means weekdays at 09:00. Do not convert to UTC.",
      ),
    delayMinutes: z
      .number()
      .int()
      .positive()
      .max(525_600)
      .nullable()
      .optional()
      .describe(
        "For any relative delay from now — 'in 3 minutes' (3), 'in 2 hours' (120), 'later'/'later' — set the exact positive delay in whole minutes and omit cron. The host calculates the future local schedule from its real current clock, so never compute an absolute time or cron yourself. For an absolute named date/time or a recurring schedule, omit it (or set null) and provide cron.",
      ),
    prompt: nonEmptyString.describe(
      "Complete prompt to send at every scheduled fire. Include all instructions needed when the automation runs. Describe the final work directly; do not ask it to create or schedule another automation or call CronCreate.",
    ),
    title: nonEmptyString.describe(
      "Concise automation title that preserves the user's natural-language schedule phrase verbatim. For example, for 'Remind me to drink water every 20 minutes', use 'Drink water reminder every 20 minutes', not 'Drink water reminder'.",
    ),
    recurring: z
      .boolean()
      .optional()
      .describe(
        "true (default) repeats until paused or deleted. false creates a finite automation; without maxRuns it runs once.",
      ),
    maxRuns: z
      .number()
      .int()
      .positive()
      .optional()
      .describe(
        "Maximum successful scheduled dispatch count. Use only with recurring=false; omit for a one-shot automation (defaults to 1).",
      ),
// Session side "custom repeat" carrier. Every N minutes/hours/days/weeks/months/years is the same as UI by
// intervalUnit + interval means that the real interval is carried by host scheduleRule; cron is only a legal compatible display.
// The cron field step limit (minute 59, hour 24, day-of-month 31, month 12) cannot be allowed to tamper with the true frequency.
// carrier and delayMinutes (one-shot) are mutually exclusive.
    intervalUnit: z
      .enum(["minute", "hourly", "daily", "weekly", "monthly", "yearly"])
      .optional()
      .describe(
        "Custom recurring interval unit for every N minutes/hours/days/weeks/months/years. Pair with interval (1-200) for every N-unit request, even if cron can express N; submit a legal compatible cron whose time/day slots are used by the scheduleRule. Omit both for ordinary calendar cron schedules.",
      ),
    interval: z
      .number()
      .int()
      .min(1)
      .max(200)
      .optional()
      .describe(
        "Integer interval from 1 to 200 paired with intervalUnit. The host carries the real interval via scheduleRule; the compatible cron is only a legal display expression. Must be set together with intervalUnit.",
      ),
  })
  .strict()
  .refine((input) => hasRelativeDelayMinutes(input) || input.cron !== undefined, {
    message: "cron is required when delayMinutes is not set",
    path: ["cron"],
  })
  .refine((input) => !hasRelativeDelayMinutes(input) || input.cron === undefined, {
    message: "a relative delay must omit cron; the host computes the schedule from its real clock",
    path: ["cron"],
  })
  .refine((input) => !hasRelativeDelayMinutes(input) || input.recurring !== true, {
    message: "a relative delay creates a one-shot automation and cannot use recurring=true",
    path: ["recurring"],
  })
  .refine((input) => !hasRelativeDelayMinutes(input) || input.maxRuns === undefined, {
    message: "a relative delay runs once and cannot use maxRuns",
    path: ["maxRuns"],
  })
// intervalUnit+interval is a periodic carrier, which conflicts semantically with one-time relative delay delayMinutes
// (Periodic vs one-time); simultaneous interpretation will form a contradictory state and must be rejected at the contract layer (the service layer also has domain defense).
  .refine((input) => input.intervalUnit === undefined || !hasRelativeDelayMinutes(input), {
    message: "intervalUnit is a recurring carrier and cannot combine with a relative delayMinutes",
    path: ["intervalUnit"],
  })
// intervalUnit and interval must be paired: passing only one of them cannot determine the real interval, so it is rejected.
  .refine((input) => (input.intervalUnit === undefined) === (input.interval === undefined), {
    message: "intervalUnit and interval must be set together",
    path: ["interval"],
  })
// The definition of carrier is a long-period infinite loop; one-time/limited times conflicts with its scheduling semantics.
  .refine((input) => input.intervalUnit === undefined || input.recurring !== false, {
    message: "intervalUnit is a recurring carrier and requires recurring=true",
    path: ["recurring"],
  })
  .refine((input) => input.intervalUnit === undefined || input.maxRuns === undefined, {
    message: "intervalUnit is a recurring carrier and cannot combine with maxRuns",
    path: ["maxRuns"],
  });
export type CronCreateInput = z.infer<typeof CronCreateInputSchema>;
export const CronCreateInputJsonSchema = toToolJsonSchema(CronCreateInputSchema);

const CronUpdateInputObjectSchema = z
  .object({
    id: nonEmptyString.describe("Automation id returned by CronCreate or CronList"),
    cron: nonEmptyString
      .optional()
      .describe(
        "Replacement standard 5-field cron expression in the user's local timezone. Omit to preserve the existing schedule. Do not convert to UTC.",
      ),
    prompt: nonEmptyString
      .optional()
      .describe(
        "Replacement prompt for future scheduled fires. Omit to preserve the existing prompt.",
      ),
// When title is optional, the model only changes cron/prompt and leaves the old time or old task semantics in the title.
// Session updates must explicitly commit the final header, so that the same in-place update can be persisted synchronously and echo consistent results.
    title: nonEmptyString.describe(
      "Required synchronized automation title describing the task after this update. Keep the user's natural-language schedule phrase consistent with cron (for example, changing every 5 minutes to every 6 minutes must also change the title), and update the title when the prompt meaning changes.",
    ),
    recurring: z
      .boolean()
      .optional()
      .describe(
        "Replacement recurrence mode. true repeats indefinitely and clears any old finite maxRuns limit; false is finite. Do not combine true with a numeric maxRuns.",
      ),
    maxRuns: z
      .number()
      .int()
      .positive()
      .nullable()
      .optional()
      .describe(
        "Replacement maximum successful scheduled dispatch count for recurring=false. null clears the existing limit and is valid only when recurring=true is included in the same update; when recurring=true is supplied without maxRuns, the service clears the old limit automatically.",
      ),
// Customized repeat carrier, the semantics are the same as intervalUnit+interval on the create side (every N units are executed uniformly through scheduleRule).
    intervalUnit: z
      .enum(["minute", "hourly", "daily", "weekly", "monthly", "yearly"])
      .optional()
      .describe(
        "Switch this automation to a long recurring interval whose step exceeds a cron field ceiling (hourly N>24, daily N>31, etc.). Pair with interval and submit a legal compatible cron (omit cron to keep the existing schedule's minute).",
      ),
    interval: z
      .number()
      .int()
      .min(1)
      .max(200)
      .optional()
      .describe(
        "Positive integer interval paired with intervalUnit. Must be set together with intervalUnit.",
      ),
  })
  .strict();

const CRON_UPDATE_FIELDS = [
  "cron",
  "prompt",
  "title",
  "recurring",
  "maxRuns",
  "intervalUnit",
  "interval",
] as const;
export const CronUpdateInputSchema = CronUpdateInputObjectSchema.refine(
  (input) => CRON_UPDATE_FIELDS.some((field) => input[field] !== undefined),
  { message: "CronUpdate requires at least one field to update" },
)
  .refine((input) => input.maxRuns !== null || input.recurring === true, {
    message: "Clearing maxRuns requires recurring=true in the same update",
    path: ["maxRuns"],
  })
  .refine((input) => input.recurring !== true || typeof input.maxRuns !== "number", {
    message: "recurring=true cannot be combined with a numeric maxRuns",
    path: ["maxRuns"],
  })
// intervalUnit and interval must be paired (same as create side semantics).
  .refine((input) => (input.intervalUnit === undefined) === (input.interval === undefined), {
    message: "intervalUnit and interval must be set together",
    path: ["interval"],
  })
// The carrier will switch the historical one-time task to an infinite loop, and the caller cannot simultaneously convey contradictory limited-time semantics.
  .refine((input) => input.intervalUnit === undefined || input.recurring !== false, {
    message: "intervalUnit is a recurring carrier and cannot combine with recurring=false",
    path: ["recurring"],
  })
  .refine(
    (input) =>
      input.intervalUnit === undefined ||
      input.maxRuns === undefined ||
      (input.maxRuns === null && input.recurring === true),
    {
      message:
        "intervalUnit is a recurring carrier and only allows maxRuns=null with recurring=true",
      path: ["maxRuns"],
    },
  );
export type CronUpdateInput = z.infer<typeof CronUpdateInputSchema>;
// provider-visible schema used top-level anyOf to express recurring/maxRuns combination constraints.
// Directly conflicts with core's cross-provider guard (forbids provider-internal keys such as $ref/$defs/anyOf);
// anyOf also leads to the related problems of "some provider read-only branches are required and top-level id/title is missed".
// The provider only projects clean object schema (top-level required takes effect naturally), and the combined invariants are given by
// Refine enforcement of runtime CronUpdateInputSchema, model side relies on recurring/maxRuns
// description Tip, illegal combinations will be rejected and an explicit error will be returned when the tool is executed.
export const CronUpdateInputJsonSchema = toToolJsonSchema(CronUpdateInputObjectSchema);

export const CronListInputSchema = z.object({}).strict();
export type CronListInput = z.infer<typeof CronListInputSchema>;
export const CronListInputJsonSchema = toToolJsonSchema(CronListInputSchema);

export const CronDeleteInputSchema = z
  .object({
    id: nonEmptyString.describe("Automation id returned by CronCreate or CronList"),
  })
  .strict();
export type CronDeleteInput = z.infer<typeof CronDeleteInputSchema>;
export const CronDeleteInputJsonSchema = toToolJsonSchema(CronDeleteInputSchema);

/**
* Customize the contract layer image schema of the repetition rules (zod v3).
* Synchronize with the ZCodeAutomationScheduleRule structure of @zcode/shared - no import shared here,
* Avoid cross-dependency between agent contracts' zod v3 and shared zod v4 (same as browser-control mirroring convention).
* cronExpr is reserved for compatible display, and scheduling uses this field as the authority; the long interval carrier on the session side is carried by this field after normalization.
 */
export const CronAutomationScheduleRuleSchema = z
  .object({
    unit: z.enum(["minute", "hourly", "daily", "weekly", "monthly", "yearly"]),
    interval: z.number().int().positive(),
    hour: z.number().int().min(0).max(23),
    minute: z.number().int().min(0).max(59),
    anchorAt: z.number().int(),
    weekdays: z.array(z.number().int().min(0).max(6)).optional(),
    monthDays: z.array(z.number().int().min(1).max(31)).optional(),
/** yearly uses: 1-12 human months. Default fallback to the month of anchorAt (compatible with old records that do not write this field). */
    months: z.array(z.number().int().min(1).max(12)).optional(),
    monthlyMode: z.enum(["date", "weekday"]).optional(),
  })
  .strict();
export type CronAutomationScheduleRule = z.infer<typeof CronAutomationScheduleRuleSchema>;

export const CronAutomationSchema = z
  .object({
    automationId: nonEmptyString,
    title: z.string(),
    cronExpr: nonEmptyString,
    prompt: nonEmptyString,
    enabled: z.boolean(),
    lifecycleStatus: z.enum(["active", "completed", "failed", "paused"]),
    nextRunAt: z.number().int().nonnegative().optional(),
    lastRunAt: z.number().int().nonnegative().optional(),
    runCount: z.number().int().nonnegative(),
    recurring: z.boolean(),
    maxRuns: z.number().int().positive().optional(),
    modelSelection: cronModelSelectionSchema.optional(),
    mode: z.enum(["build", "edit", "plan", "yolo"]).optional(),
// Customize repetition rules; by default, scheduling falls back to parsing cronExpr. Conversation cards must read this field before they can be displayed.
// Real intervals that cron cannot express (e.g. every 50 hours, every 40 days, compatible with cronExpr is just 0 * * * *).
    scheduleRule: CronAutomationScheduleRuleSchema.optional(),
  })
  .strict();
export type CronAutomation = z.infer<typeof CronAutomationSchema>;

export const CronCreateOutputSchema = z
  .object({
    automation: CronAutomationSchema,
    message: nonEmptyString,
  })
  .strict();
export type CronCreateOutput = z.infer<typeof CronCreateOutputSchema>;
export const CronCreateOutputJsonSchema = toToolJsonSchema(CronCreateOutputSchema);

export const CronUpdateOutputSchema = z
  .object({
    automation: CronAutomationSchema,
    message: nonEmptyString,
  })
  .strict();
export type CronUpdateOutput = z.infer<typeof CronUpdateOutputSchema>;
export const CronUpdateOutputJsonSchema = toToolJsonSchema(CronUpdateOutputSchema);

export const CronListOutputSchema = z
  .object({
    automations: z.array(CronAutomationSchema),
  })
  .strict();
export type CronListOutput = z.infer<typeof CronListOutputSchema>;
export const CronListOutputJsonSchema = toToolJsonSchema(CronListOutputSchema);

export const CronDeleteOutputSchema = z
  .object({
    deleted: z.boolean(),
    id: nonEmptyString,
    message: nonEmptyString,
  })
  .strict();
export type CronDeleteOutput = z.infer<typeof CronDeleteOutputSchema>;
export const CronDeleteOutputJsonSchema = toToolJsonSchema(CronDeleteOutputSchema);
