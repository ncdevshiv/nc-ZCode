import type {
  CompactPhase,
  CompactReason,
  MessageId,
  ModelStreamRecoveryStatus,
  Model,
  OutputStylePromptConfig,
  SessionEvent,
  TraceContext,
  TraceId,
  TurnId,
} from "../deps.js";
import type { ActiveTurnSteeringState } from "../types.js";
import type { SubagentRunOptions } from "@zcode/contracts";
import type { DrainedPendingInputDiagnostics } from "../types.js";
import type { TurnMachineImpl } from "../deps.js";
import type { RuntimeMessageEntry } from "../../agent/message-history.js";

export type PendingStreamRecoveryRequest = ModelStreamRecoveryStatus;

export const RAPID_REFILL_TOOL_TURN_THRESHOLD = 3;
export const MAX_CONSECUTIVE_RAPID_REFILLS = 3;
export const AUTOMATION_MUTATION_TOOL_NAMES = ["CronCreate", "CronUpdate", "CronDelete"] as const;
const AUTOMATION_QUERY_ID_PREFIX = "automation-";
/**
* Tools hidden by the idle distribution wheel; OffPeakList is read-only and reserved.
* - OffPeakCreate: Prevent idle-time tasks from recursively self-derivating and infinite scheduling.
* - SendMessage / Workflow: Will restart the child Agent outside the modelExecution of idle turn (SendMessage continues to run
* Completed sub-Agent, Workflow derived script sub-session), model building based on parent session resident selection.
 *
* Independent constant, never merged into AUTOMATION_MUTATION_TOOL_NAMES - explicitly released by cron automation turn
* OffPeakCreate (scheduled idle time task), mixed in will cause automation turn to deny falsely.
 */
export const OFF_PEAK_MUTATION_TOOL_NAMES = ["OffPeakCreate", "SendMessage", "Workflow"] as const;
// The init segment is dispatched during idle time. The traceId has no fixed prefix. Only the resume segment is `${offPeakTaskId}:resume:*`
// (starting with offpeak-); the prefix is ​​just the resume signal, and the main signal must be explicit offPeakTaskId.
const OFF_PEAK_QUERY_ID_PREFIX = "offpeak-";

export interface CompactLoopTracking {
  consecutiveRapidRefills: number;
  toolTurnsSinceCompact: number;
}

export interface RapidRefillDecision {
  consecutiveRapidRefills: number;
  shouldBlock: boolean;
  toolTurnsSinceCompact: number;
}

export type CompactAttemptOutcome = "skipped" | "compacted" | "failed";

export type AutoCompactOutcome = CompactAttemptOutcome | "rapid_refill_blocked";

export interface AutoCompactLoopContext {
  compactReason: CompactReason;
  modelStepIndex: number;
  phase: CompactPhase;
  rapidRefill: RapidRefillDecision;
  model: Model;
  turnRequestState: TurnRequestState;
}

export interface ReactiveCompactLoopContext {
  activeEntries?: readonly RuntimeMessageEntry[];
  modelStepIndex: number;
  model: Model;
  rapidRefillCount: number;
  turnRequestState: TurnRequestState;
}

export interface TurnRequestState {
  entries: readonly RuntimeMessageEntry[];
  outputTokenContinuationCount: number;
}

export interface RegularTurnLoopState {
  activeTurn?: ActiveTurnSteeringState;
/** Host admission's explicitly passed automation identity for this cycle; cannot be inferred from persistent task metadata. */
  automationId?: string;
/** The idle task identity of this round passed explicitly by Host admission; mutually exclusive with automationId, not inferred from persistent meta. */
  offPeakTaskId?: string;
/** After CronCreate hits the global upper limit, this user's turn will be permanently switched to plain text reply, prohibiting the model from restoring itself. */
  automationCreateLimitReached?: boolean;
  anomalyWarningsInjected: number;
/** Whether the background result notification sourced from subagent has been consumed in this round. */
  backgroundSubagentResultConsumed: boolean;
/** Whether the background notification sourced from workflow (dynamic-workflow run) has been consumed this round. */
  workflowResultConsumed: boolean;
  compactTracking?: CompactLoopTracking;
  currentUserMessageId: MessageId;
  drainedSteerForNextRequest?: DrainedPendingInputDiagnostics;
  events: SessionEvent[];
  input: string;
  modelResponse: string;
/** A callable model that is fixed for this round; configuration changes only affect loops created in the future. */
  model: Model;
/** execution means that the current Active Model cannot be overwritten by the guide in the same loop. */
  modelSelectionScope?: "execution";
/** Core Server's front-end child Selection override; takes precedence over profile and parent model inheritance. */
  subagentModelOverride?: SubagentRunOptions["modelOverride"];
  modelStepCount: number;
/** The number of assistant/compact products that the current query has successfully written to the provider's visible persistent history. */
  historyRoundCount: number;
  reactiveCompactAttemptedInCurrentModelStep: boolean;
  repeatedToolCallSignature?: string;
  repeatedToolCallStreakCount: number;
  pendingStreamRecoveryRequest?: PendingStreamRecoveryRequest;
  stopHookContinuationCount: number;
/** The starting point of the raw transcript of the final successful product turn; it is assigned a value only when it is confirmed that it will no longer continue. */
  stableProductStartMessageId?: MessageId;
/** Final success assistant boundary; appears in pairs with stableProductStartMessageId. */
  stableBoundaryAssistantMessageId?: MessageId;
  streamRecoveryRetryCount: number;
  tokenCount: number;
  toolCallCount: number;
/** Provider-local history of the current Turn; released directly after the Turn ends. */
  turnRequestState: TurnRequestState;
/** The current turn tool name is not exposed to the provider; the registry is still reserved for in-depth verification of execution boundaries. */
  toolDisallowlist?: readonly string[];
  traceId: TraceId;
  turnAbortSignal: AbortSignal;
  turnId: TurnId;
  turnMachine: TurnMachineImpl;
/** Provider-visible output style currently captured by Turn; does not represent explicit subagent model override. */
  turnOutputStyle?: OutputStylePromptConfig;
  turnTraceContext: TraceContext;
  userMessageId: MessageId;
}

export function isAutomationMutationRestrictedTurn(state: RegularTurnLoopState): boolean {
  if (state.automationId?.trim()) return true;
  if (state.turnTraceContext.queryId?.trim().startsWith(AUTOMATION_QUERY_ID_PREFIX)) return true;

  const disallowedTools = new Set(state.toolDisallowlist ?? []);
// active/busy automation input will merge the turn-scoped denylist into the current loop; even if the original
// automationId is no longer the first input of the loop, and the same fact must continue to be passed to the handler execution boundary.
  return AUTOMATION_MUTATION_TOOL_NAMES.every((toolName) => disallowedTools.has(toolName));
}

/**
* Whether this round is automatically dispatched during idle time (need to deny OffPeakCreate). triple signal vs.
* isAutomationMutationRestrictedTurn isomorphism: explicit offPeakTaskId is the main signal;
* The traceId prefix of the resume segment and the turn denylist are in depth.
 */
export function isOffPeakCreateRestrictedTurn(state: RegularTurnLoopState): boolean {
  if (state.offPeakTaskId?.trim()) return true;
  if (state.turnTraceContext.queryId?.trim().startsWith(OFF_PEAK_QUERY_ID_PREFIX)) return true;

// Only recognize the Sentinel OffPeakCreate: The denylist distributed by the old host may not have the new tools yet.
  const disallowedTools = new Set(state.toolDisallowlist ?? []);
  return disallowedTools.has(OFF_PEAK_MUTATION_TOOL_NAMES[0]);
}

export function evaluateRapidRefill(
  tracking: CompactLoopTracking | undefined,
): RapidRefillDecision {
  const toolTurnsSinceCompact = tracking?.toolTurnsSinceCompact ?? 0;
  const consecutiveRapidRefills =
    tracking && toolTurnsSinceCompact < RAPID_REFILL_TOOL_TURN_THRESHOLD
      ? tracking.consecutiveRapidRefills + 1
      : 0;

  return {
    consecutiveRapidRefills,
    shouldBlock: consecutiveRapidRefills >= MAX_CONSECUTIVE_RAPID_REFILLS,
    toolTurnsSinceCompact,
  };
}

export function recordCompactSuccess(
  state: RegularTurnLoopState,
  decision: RapidRefillDecision,
): void {
  state.compactTracking = {
    consecutiveRapidRefills: decision.consecutiveRapidRefills,
    toolTurnsSinceCompact: 0,
  };
}

export function recordCompletedToolBatch(state: RegularTurnLoopState): void {
// The old guard lives in the entire user turn and remains used after the complete tool batch ends, resulting in the subsequent real overflow being unable to reactive compact again.
  state.reactiveCompactAttemptedInCurrentModelStep = false;
  if (state.compactTracking) {
    state.compactTracking.toolTurnsSinceCompact += 1;
  }
}

export function recordModelHistoryRound(state: RegularTurnLoopState): void {
// toolCallCount will expand the parallel tools by number, and cannot express the rounds in which the model is actually written into history.
// The call point follows the submission boundary of the existing modelStepCount and accumulates additional historical rounds without changing the loop control semantics.
  state.historyRoundCount += 1;
}

export function recordCompactHistoryRound(state: RegularTurnLoopState): void {
// compact summary is an independent provider visible to the persistent history, but it is not an ordinary model step and is accumulated once.
  state.historyRoundCount += 1;
}
