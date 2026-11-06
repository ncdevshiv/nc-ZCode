// ============================================================
//Status section of the port →Status section output by GetWorkflowRun
// ============================================================
//
// The fields on both sides have the same name and are synonymous. Here we still move them field by field instead of transparently transmitting them as they are: the output schema is strict, and the port date is
// If there is one more field, transparent transmission will cause the entire tool call to fail at the runtimeOutputSchema verification. Moving field by field is equivalent to moving
// "What can be seen in the model" is written in one place, and port evolution will not change it from behind.
//
// Optional fields are always `...(x === undefined ? {} : { x })`: an explicit `undefined` value is the same as the absence of
// `toEqual` and JSON serialization are two different things, and the contract on the model side is "If you don't know, you don't have this key."

import type {
  DynamicWorkflowRunHealth,
  DynamicWorkflowRunPhaseView,
  DynamicWorkflowRunSubagentAsk,
  DynamicWorkflowRunSubagentView,
  DynamicWorkflowRunSubagentWait,
  GetWorkflowRunHealth,
  GetWorkflowRunPhase,
  GetWorkflowRunSubagent,
  GetWorkflowRunSubagentAsk,
} from "@zcode/contracts";
import { GET_WORKFLOW_RUN_ROSTER_LIMITS } from "@zcode/contracts";

export function toGetWorkflowRunPhases(
  phases: readonly DynamicWorkflowRunPhaseView[] | undefined,
): GetWorkflowRunPhase[] | undefined {
// The script has no declared stages and has not entered any of them ⇒ The entire field is absent (an empty array reads like "the stage table is empty").
  if (phases === undefined || phases.length === 0) return undefined;
  return phases.slice(0, GET_WORKFLOW_RUN_ROSTER_LIMITS.maxPhases).map((phase) => ({
    name: phase.name,
    state: phase.state,
    rounds: phase.rounds,
    nodesSettled: phase.nodesSettled,
    nodesRunning: phase.nodesRunning,
    ...(phase.enteredAt === undefined ? {} : { enteredAt: phase.enteredAt }),
    ...(phase.exitedAt === undefined ? {} : { exitedAt: phase.exitedAt }),
  }));
}

/**
* The roster is truncated by {@link GET_WORKFLOW_RUN_ROSTER_LIMITS.maxSubagents} and says "truncated":
* A roster that is silently missing a dozen lines reads like "this run only has 64 subagents", which is a lie.
 */
export function toGetWorkflowRunSubagents(subagents: readonly DynamicWorkflowRunSubagentView[]): {
  subagents: GetWorkflowRunSubagent[];
  truncated: boolean;
} {
  const kept = subagents.slice(0, GET_WORKFLOW_RUN_ROSTER_LIMITS.maxSubagents);
  return {
    subagents: kept.map((subagent) => ({
      siteId: subagent.siteId,
      ordinal: subagent.ordinal,
      ...(subagent.name === undefined ? {} : { name: subagent.name }),
      state: subagent.state,
      ...(subagent.phaseName === undefined ? {} : { phaseName: subagent.phaseName }),
      ...(subagent.currentAsk === undefined
        ? {}
        : { currentAsk: toCurrentAsk(subagent.currentAsk) }),
      ...(subagent.wait === undefined ? {} : { wait: toWait(subagent.wait) }),
      ...(subagent.parkedOn === undefined ? {} : { parkedOn: subagent.parkedOn }),
      stepsSettled: subagent.stepsSettled,
      stepsFailed: subagent.stepsFailed,
      tokens: subagent.tokens,
      ...(subagent.lastProgressAt === undefined ? {} : { lastProgressAt: subagent.lastProgressAt }),
    })),
    truncated: kept.length < subagents.length,
  };
}

function toCurrentAsk(ask: DynamicWorkflowRunSubagentAsk): GetWorkflowRunSubagentAsk {
  return {
    siteId: ask.siteId,
    ordinal: ask.ordinal,
    ...(ask.actorSeq === undefined ? {} : { actorSeq: ask.actorSeq }),
    ...(ask.instructionsHead === undefined ? {} : { instructionsHead: ask.instructionsHead }),
    ...(ask.startedAt === undefined ? {} : { startedAt: ask.startedAt }),
// turn / toolCalls Absence is read as "don't know", `0` is read as "not a single tool has been adjusted" - old journal
// Without node-progress, these two things must be distinguishable, so never `?? 0` here.
    ...(ask.turn === undefined ? {} : { turn: ask.turn }),
    ...(ask.toolCalls === undefined ? {} : { toolCalls: ask.toolCalls }),
    ...(ask.lastTool === undefined
      ? {}
      : {
          lastTool: {
            name: ask.lastTool.name,
            ...(ask.lastTool.target === undefined ? {} : { target: ask.lastTool.target }),
            ...(ask.lastTool.at === undefined ? {} : { at: ask.lastTool.at }),
          },
        }),
  };
}

function toWait(wait: DynamicWorkflowRunSubagentWait): NonNullable<GetWorkflowRunSubagent["wait"]> {
  return {
    cause: wait.cause,
    ...(wait.reason === undefined ? {} : { reason: wait.reason }),
    ...(wait.retryAfterMs === undefined ? {} : { retryAfterMs: wait.retryAfterMs }),
    ...(wait.since === undefined ? {} : { since: wait.since }),
  };
}

export function toGetWorkflowRunHealth(health: DynamicWorkflowRunHealth): GetWorkflowRunHealth {
  return {
    ...(health.lastProgressAt === undefined ? {} : { lastProgressAt: health.lastProgressAt }),
    ...(health.stalledSince === undefined ? {} : { stalledSince: health.stalledSince }),
    ...(health.concurrency === undefined
      ? {}
      : {
          concurrency: {
            effective: health.concurrency.effective,
            cap: health.concurrency.cap,
            ...(health.concurrency.reason === undefined
              ? {}
              : { reason: health.concurrency.reason }),
            ...(health.concurrency.since === undefined ? {} : { since: health.concurrency.since }),
          },
        }),
    consecutiveFailures: health.consecutiveFailures,
    cachedSteps: health.cachedSteps,
// Absent when 0: final run with no remaining lines is the norm, and `leftover_running=0` reads like an event that happened.
    ...(health.leftoverRunning === undefined || health.leftoverRunning === 0
      ? {}
      : { leftoverRunning: health.leftoverRunning }),
    pendingQuestionsKnown: health.pendingQuestionsKnown,
  };
}
