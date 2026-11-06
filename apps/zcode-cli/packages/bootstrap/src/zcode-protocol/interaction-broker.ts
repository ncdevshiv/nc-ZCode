/* eslint-disable max-lines -- Cohesive contract/runtime module kept in one file; split is tracked separately. */
import { raceClientRequestWithV4Interaction } from "./interaction-response-race.js";
import {
  ASK_USER_QUESTION_TOOL_NAME,
  AskUserQuestionInputSchema,
  AMEND_WORKFLOW_TOOL_NAME,
  CREATE_WORKFLOW_TOOL_NAME,
  EXIT_PLAN_MODE_TOOL_NAME,
  SESSION_ENTRY_USER_INPUT_AUTO_RESOLUTION,
  type AskUserQuestion,
  type PermissionBrokerPort,
  type PermissionBrokerRequest,
  type PermissionBrokerRequestOptions,
  type PermissionBrokerResult,
} from "@zcode/contracts";
import {
  WORKFLOW_REFINE_PERMISSION_OPTION_ID,
  zcodePermissionResponseSchema,
  zcodeProtocolMethods,
  zcodeUserInputResponseSchema,
  type ZCodePermissionOption,
  type ZCodePermissionResponse,
  type ZCodeUserInputQuestion,
  type ZCodeUserInputResponse,
} from "@zcode/shared";
import type {
  V4InteractionAnswer,
  V4InteractionRegistrationOptions,
} from "../zcode-protocol-v4/interaction-registry.js";
import type { ZCodeProtocolAgentServerContext } from "./server-types.js";
import {
  buildProtocolPermissionOptions,
  buildSessionPermissionUpdates,
  SESSION_ALLOW_PERMISSION_OPTION_KIND,
  toLegacyPermissionOptionsPolicy,
  buildPermissionDeniedContent,
  PERMISSION_DENIED_BY_USER_CONTENT,
} from "./permission-options.js";

const EXIT_PLAN_MODE_APPROVAL_QUESTION = "Review this implementation plan.";
const EXIT_PLAN_MODE_APPROVAL_APPROVE = "approve";
const INTERACTION_REQUEST_REANNOUNCE_INTERVAL_MS = 1_000;

export function createProtocolInteractionBroker(
  context: ZCodeProtocolAgentServerContext,
): PermissionBrokerPort {
  return {
    requestPermission(request, options) {
      if (request.toolName === ASK_USER_QUESTION_TOOL_NAME) {
        return requestUserInput(context, request, options);
      }
      if (request.toolName === EXIT_PLAN_MODE_TOOL_NAME) {
        return requestExitPlanModeApproval(context, request, options);
      }
      return requestPermission(context, request, options);
    },
  };
}

async function requestPermission(
  context: ZCodeProtocolAgentServerContext,
  request: PermissionBrokerRequest,
  options?: PermissionBrokerRequestOptions,
): Promise<PermissionBrokerResult> {
  const permissionOptions = buildProtocolPermissionOptions(request);
// v3 reverse RPC option list: session confirmation-free is only delivered in v4 (the old desktop returns the original response text and cannot recognize the session semantics).
  const legacyPermissionOptions = buildProtocolPermissionOptions({
    ...request,
    optionsPolicy: toLegacyPermissionOptionsPolicy(request.optionsPolicy),
  });
  const response = await raceClientRequestWithV4Interaction(
    context,
    request.requestId,
    options?.signal,
    (signal) =>
      context.requestClient(
        zcodeProtocolMethods.interactionRequestPermission,
        {
          input: request.input,
          reason: request.reason,
          requestId: request.requestId,
          riskLevel: request.riskLevel,
          sessionId: request.sessionId,
          ...(request.origin ? { origin: request.origin } : {}),
          options: legacyPermissionOptions,
          toolCallId: request.toolCallId,
          toolName: request.toolName,
          turnId: request.turnId,
        },
        zcodePermissionResponseSchema,
        withInteractionRequestRecovery(options, signal),
      ),
// v4 answer → ZCodePermissionResponse: optionId semantics synthesized from v4 reducer
// allowOnce/allowAlways/deny (see product-projection onPermissionRequested).
    (answer) => {
      const response = v4AnswerToPermissionResponse(answer, permissionOptions, request.toolName);
      return response.decision === "deny" && answer.freeText?.trim()
        ? { ...response, preserveReasonFormatting: true }
        : response;
    },
    {
      ...createInteractionRegistrationOptions(request, "other"),
      ...(!request.origin &&
      !request.optionsPolicy &&
      options?.claimResponse &&
      context.deps?.sessionStore?.commitPermissionFullAccess
        ? {
            fullAccess: async () => {
              if (!options.claimResponse!()) throw new Error("Permission response already settled");
              options.signal?.throwIfAborted();
              const record = context.sessions.get(String(request.sessionId));
              if (!record) throw new Error("Permission session unavailable");
              const eventId = await record.app.runtime.grantPermissionFullAccess(
                request.requestId,
                options.signal,
              );
              await context.v4Gateway?.waitForPermissionGrantCommit(
                String(request.sessionId),
                eventId,
              );
            },
          }
        : {}),
    },
  );
  return {
    ...response,
// Compatibility reason: legacy client allows reason to be omitted; ordinary users still need to make it clear to the model that the tool is not executed if they refuse.
// Otherwise core will fall back to the generic `Permission denied for <tool>`, which cannot prevent bypass attempts.
    ...(response.decision === "deny" && !response.reason?.trim()
      ? { reason: PERMISSION_DENIED_BY_USER_CONTENT }
      : {}),
    resolvedAt: new Date(),
  };
}

/**
* v4 permission response mapping: Prioritize exact matching of buildProtocolPermissionOptions by optionId
* Synthetic options (allow_project carries permissionUpdates persistence rules and cannot be lost); projection side
* Synthetic allowAlways semantically equivalent to allow_project. Unknown optionId Press deny to reveal the details - permissions
* Semantics would rather reject than allow unknown responses.
 *
* workflow Refine: This option is only available in v4 projection
* Synthetic, does not enter the legacy option list, so special judgment is made before exact matching. freeText is empty or not
* The CreateWorkflow tool forges the optionId and falls into the existing deny without reasonSource——
* The channel where feedback is upgraded to user message must only be open to real user input.
 */
function v4AnswerToPermissionResponse(
  answer: V4InteractionAnswer,
  permissionOptions: ZCodePermissionOption[],
  toolName: string,
): ZCodePermissionResponse & {
  reasonSource?: PermissionBrokerResult["reasonSource"];
  sessionPermissionUpdates?: PermissionBrokerResult["sessionPermissionUpdates"];
} {
  const refineFeedback = answer.freeText?.trim();
  if (
    (toolName === CREATE_WORKFLOW_TOOL_NAME || toolName === AMEND_WORKFLOW_TOOL_NAME) &&
    answer.optionId === WORKFLOW_REFINE_PERMISSION_OPTION_ID &&
    refineFeedback
  ) {
    return {
      decision: "deny",
      reason: refineFeedback,
      reasonSource: "workflow_refine_feedback",
    };
  }
  const exact = permissionOptions.find((option) => option.optionId === answer.optionId);
  if (exact) {
    if (exact.kind === "deny") {
      return { decision: "deny", reason: buildPermissionDeniedContent(answer.freeText) };
    }
// Confirmation-free session: session semantics are synthesized here rather than put in
// option.response——zcodePermissionUpdateSchema on wire is strict. If there is an additional field on the old desktop, the event will be lost.
    if (exact.kind === SESSION_ALLOW_PERMISSION_OPTION_KIND) {
      return {
        ...exact.response,
        sessionPermissionUpdates: buildSessionPermissionUpdates(toolName),
      };
    }
    return exact.response;
  }
  if (answer.optionId === "allowAlways") {
    const allowAlways = permissionOptions.find((option) => option.kind === "allow_always");
    if (allowAlways) {
      return allowAlways.response;
    }
  }
  if (answer.optionId === "allowOnce") {
    return { decision: "allow", reason: "Approved once" };
  }
// deny/rejectOnce/rejectAlways, unknown optionId, no optionId all deny.
  return { decision: "deny", reason: buildPermissionDeniedContent(answer.freeText) };
}

async function requestUserInput(
  context: ZCodeProtocolAgentServerContext,
  request: PermissionBrokerRequest,
  options?: PermissionBrokerRequestOptions,
): Promise<PermissionBrokerResult> {
  const parsed = AskUserQuestionInputSchema.safeParse(request.input);
  if (!parsed.success) {
    return {
      decision: "deny",
      reason: `Invalid AskUserQuestion input: ${
        parsed.error.issues[0]?.message ?? "schema validation failed"
      }`,
      resolvedAt: new Date(),
    };
  }

  const initialAutoResolution = await readPersistedAutoResolution(context, request);

  const response = await raceClientRequestWithV4Interaction(
    context,
    request.requestId,
    options?.signal,
    (signal) =>
      context.requestClient(
        zcodeProtocolMethods.interactionRequestUserInput,
        {
          input: request.input,
          prompt: request.reason,
          questions: parsed.data.questions.map(mapAskUserQuestion),
          requestId: request.requestId,
          schema: { toolName: request.toolName },
          sessionId: request.sessionId,
          ...(request.origin ? { origin: request.origin } : {}),
          toolCallId: request.toolCallId,
          toolName: request.toolName,
          turnId: request.turnId,
        },
        zcodeUserInputResponseSchema,
        withInteractionRequestRecovery(options, signal),
      ),
// v4 answer AskUserQuestion: freeText/optionId falls into the single question answer slot
// (content.answer compatible path of normalizeAskUserQuestionResponseContent);
// deny decline. Multi-question scenes, etc. are accurately mapped after v4 projection modeling userInput kind.
    (answer) => v4AnswerToUserInputResponse(answer),
    createInteractionRegistrationOptions(
      request,
      "askUserQuestion",
      context,
      initialAutoResolution,
    ),
  );

  return userInputResponseToBrokerResult(request, response);
}

function v4AnswerToUserInputResponse(answer: V4InteractionAnswer): ZCodeUserInputResponse {
// answer.action exists (host adapter respondElicitation convergence path)
// According to the old respondUserInput semantics, accurate direct transmission - content carries multiple answers/annotations,
// normalizeAskUserQuestionResponseContent continues to be responsible for schema convergence.
  if (answer.action) {
    return answer.action === "accept"
      ? { action: "accept", content: answer.content ?? {} }
      : { action: answer.action };
  }
  const text = answer.freeText?.trim();
  if (text) {
    return { action: "accept", content: { answer: text } };
  }
  if (answer.optionId === "allowOnce" || answer.optionId === "allowAlways") {
    return { action: "accept", content: {} };
  }
  return { action: "decline" };
}

async function requestExitPlanModeApproval(
  context: ZCodeProtocolAgentServerContext,
  request: PermissionBrokerRequest,
  options?: PermissionBrokerRequestOptions,
): Promise<PermissionBrokerResult> {
  const response = await raceClientRequestWithV4Interaction(
    context,
    request.requestId,
    options?.signal,
    (signal) =>
      context.requestClient(
        zcodeProtocolMethods.interactionRequestUserInput,
        {
          input: request.input,
          prompt: request.reason,
          questions: [createExitPlanModeApprovalQuestion()],
          requestId: request.requestId,
          schema: { interaction: "plan_approval", toolName: request.toolName },
          sessionId: request.sessionId,
          ...(request.origin ? { origin: request.origin } : {}),
          toolCallId: request.toolCallId,
          toolName: request.toolName,
          turnId: request.turnId,
        },
        zcodeUserInputResponseSchema,
        withInteractionRequestRecovery(options, signal),
      ),
// v4 answer plan approval: allow class optionId = approval; freeText = plan feedback
// (planApprovalResponseToBrokerResult goes plan_approval_feedback deny); otherwise decline.
    (answer) => v4AnswerToPlanApprovalResponse(answer),
    createInteractionRegistrationOptions(request, "other"),
  );

  return planApprovalResponseToBrokerResult(response);
}

function v4AnswerToPlanApprovalResponse(answer: V4InteractionAnswer): ZCodeUserInputResponse {
// Same as v4AnswerToUserInputResponse——host adapter convergence path direct transmission
// action/content, planApprovalResponseToBrokerResult continue to normalize approve/feedback.
  if (answer.action) {
    return answer.action === "accept"
      ? { action: "accept", content: answer.content ?? {} }
      : { action: answer.action };
  }
  if (answer.optionId === "allowOnce" || answer.optionId === "allowAlways") {
    return {
      action: "accept",
      content: { answer: EXIT_PLAN_MODE_APPROVAL_APPROVE },
    };
  }
  const feedback = answer.freeText?.trim();
  if (feedback) {
    return { action: "accept", content: { answer: feedback } };
  }
  return { action: "decline" };
}

function createExitPlanModeApprovalQuestion(): ZCodeUserInputQuestion {
  return {
    header: "Plan",
    options: [
      {
        description: "Exit plan mode and start implementation.",
        label: "Approve",
        value: EXIT_PLAN_MODE_APPROVAL_APPROVE,
      },
    ],
    question: EXIT_PLAN_MODE_APPROVAL_QUESTION,
  };
}

function withInteractionRequestRecovery(
  options: PermissionBrokerRequestOptions | undefined,
  signal: AbortSignal,
): PermissionBrokerRequestOptions & { reannounceIntervalMs: number } {
  return {
    ...options,
// v4 racing: The internal signal has been cascaded to the outer options.signal (see raceClientRequestWithV4Interaction),
// The reverse RPC through which the dangling is canceled when the v4 reply hits.
    signal,
// The UI in the desktop/restore link may only restore pending interactions from snapshot.
// But the memory registration corresponding to the original protocol id in the host has been lost. While waiting for user response, press the same service
// requestId rediscovers the existing protocol request and allows the host to re-register the protocolRequestId that can respond.
    reannounceIntervalMs: INTERACTION_REQUEST_REANNOUNCE_INTERVAL_MS,
  };
}

function mapAskUserQuestion(question: AskUserQuestion): ZCodeUserInputQuestion {
  return {
    header: question.header,
    multiSelect: question.multiSelect,
    options: question.options.map((option) => ({
      description: option.description,
      label: option.label,
      preview: option.preview,
      value: option.label,
    })),
    question: question.question,
  };
}

function userInputResponseToBrokerResult(
  request: PermissionBrokerRequest,
  response: ZCodeUserInputResponse,
): PermissionBrokerResult {
  if (response.action !== "accept") {
    return {
      decision: "deny",
      reason:
        response.reason ??
        (response.action === "cancel"
          ? "AskUserQuestion was cancelled"
          : "AskUserQuestion was declined"),
      resolvedAt: new Date(),
    };
  }

  const input = isRecord(request.input) ? request.input : {};
  const content = normalizeAskUserQuestionResponseContent(input, response.content);
  return {
    decision: "modify",
    modifiedInput: {
      ...input,
      ...content,
    },
    reason: response.reason,
    resolvedAt: new Date(),
  };
}

function planApprovalResponseToBrokerResult(
  response: ZCodeUserInputResponse,
): PermissionBrokerResult {
  if (response.action !== "accept") {
    return {
      decision: "deny",
      reason: response.reason,
      resolvedAt: new Date(),
    };
  }

  const answer = normalizePlanApprovalAnswer(response.content);
  if (answer === EXIT_PLAN_MODE_APPROVAL_APPROVE) {
    return {
      decision: "allow",
      reason: response.reason,
      resolvedAt: new Date(),
    };
  }

  if (!answer) {
    return {
      decision: "deny",
      reason: response.reason,
      resolvedAt: new Date(),
    };
  }

  return {
    decision: "deny",
    reason: answer,
    reasonSource: "plan_approval_feedback",
    resolvedAt: new Date(),
  };
}

function normalizePlanApprovalAnswer(
  content: Record<string, unknown> | undefined,
): string | undefined {
  if (!content) {
    return undefined;
  }
  const answers = isRecord(content.answers) ? content.answers : {};
  const answer = normalizeAnswerValue(
    answers[EXIT_PLAN_MODE_APPROVAL_QUESTION] ?? content.answer_0 ?? content.answer,
  )?.trim();
  return answer && answer.length > 0 ? answer : undefined;
}

function normalizeAskUserQuestionResponseContent(
  input: Record<string, unknown>,
  content: Record<string, unknown> | undefined,
): Record<string, unknown> {
  if (!content) {
    return {};
  }

  const normalized: Record<string, unknown> = {};
  const answers = normalizeAskUserQuestionAnswers(input, content);
  if (answers) {
    normalized.answers = answers;
  }

  const annotations = normalizeAskUserQuestionAnnotations(content.annotations);
  if (annotations) {
    normalized.annotations = annotations;
  }

// To be compatible with the old single question path, the UI will submit answer_0 / answer at the same time.
//AskUserQuestionInputSchema is strict, directly merging these old fields back into the tool input will trigger
// Tool input failed inputSchema validation, so only fields explicitly allowed by the schema are retained here.
  return normalized;
}

function normalizeAskUserQuestionAnswers(
  input: Record<string, unknown>,
  content: Record<string, unknown>,
): Record<string, string> | undefined {
  const questionTexts = readAskUserQuestionTexts(input);
  if (questionTexts.length === 0) {
    return undefined;
  }

  const rawAnswers = isRecord(content.answers) ? content.answers : {};
  const answers: Record<string, string> = {};
  questionTexts.forEach((questionText, index) => {
    const rawAnswer =
      rawAnswers[questionText] ??
      content[`answer_${index}`] ??
      (questionTexts.length === 1 ? content.answer : undefined);
    const answer = normalizeAnswerValue(rawAnswer);
    if (answer !== undefined) {
      answers[questionText] = answer;
    }
  });

// action=accept + content.answers={} is the explicit success semantics of runtime automatic continuation;
// Empty objects must be preserved, and content is completely missing (old clients approved but did not provide an answer) to distinguish.
  if (isRecord(content.answers) && Object.keys(content.answers).length === 0) {
    return {};
  }
  return Object.keys(answers).length > 0 ? answers : undefined;
}

function createInteractionRegistrationOptions(
  request: PermissionBrokerRequest,
  kind: V4InteractionRegistrationOptions["kind"],
  context?: ZCodeProtocolAgentServerContext,
  initialAutoResolution?: V4InteractionRegistrationOptions["initialAutoResolution"],
): V4InteractionRegistrationOptions {
  return {
    sessionId: String(request.sessionId),
    kind,
    ...(initialAutoResolution ? { initialAutoResolution } : {}),
    ...(kind === "askUserQuestion" && context
      ? {
          onAutoResolutionUpdated: async (autoResolution) => {
            const record = context.sessions?.get(String(request.sessionId));
            if (!record) return;
            try {
              await record.app.runtime.recordUserInputAutoResolutionUpdate({
                interactionId: request.requestId,
                toolCallId: request.toolCallId,
                autoResolution,
                traceContext: {
                  ...record.traceContext,
                  traceId: request.traceId,
                  turnId: request.turnId,
                },
              });
            } catch (error) {
              context.logger?.error(
                "Failed to persist user input auto-resolution state",
                error instanceof Error ? error : new Error(String(error)),
                {
                  interactionId: request.requestId,
                  sessionId: request.sessionId,
                },
              );
            }
          },
        }
      : {}),
  };
}

async function readPersistedAutoResolution(
  context: ZCodeProtocolAgentServerContext,
  request: PermissionBrokerRequest,
): Promise<V4InteractionRegistrationOptions["initialAutoResolution"]> {
  const sessionStore = context.deps?.sessionStore;
  if (!sessionStore?.sessionEntries) return undefined;
  try {
    const entries = await sessionStore.sessionEntries({
      sessionID: request.sessionId,
      type: SESSION_ENTRY_USER_INPUT_AUTO_RESOLUTION,
    });
    const matching = entries
      .filter((entry) => {
        const data = isRecord(entry.data) ? entry.data : {};
        return (
          data.interactionId === request.requestId &&
          String(data.toolCallId ?? "") === String(request.toolCallId)
        );
      })
      .sort((left, right) => right.time.updated - left.time.updated)[0];
    if (!matching || !isRecord(matching.data)) return undefined;
    return parsePersistedAutoResolution(matching.data.autoResolution);
  } catch (error) {
    context.logger?.warn("Failed to restore user input auto-resolution state", {
      error: error instanceof Error ? error.message : String(error),
      event: "zcode_protocol.user_input_auto_resolution_restore_failed",
      interactionId: request.requestId,
      module: "bootstrap.zcode_protocol",
      sessionId: request.sessionId,
    });
    return undefined;
  }
}

function parsePersistedAutoResolution(
  value: unknown,
): V4InteractionRegistrationOptions["initialAutoResolution"] {
  if (!isRecord(value) || typeof value.startedAt !== "number") return undefined;
  if (
    (value.state === "hiddenGrace" || value.state === "visibleCountdown") &&
    typeof value.visibleAt === "number" &&
    typeof value.deadlineAt === "number"
  ) {
    return {
      state: value.state,
      startedAt: value.startedAt,
      visibleAt: value.visibleAt,
      deadlineAt: value.deadlineAt,
    };
  }
  if (value.state === "snoozed" && typeof value.snoozedAt === "number") {
    return {
      state: "snoozed",
      startedAt: value.startedAt,
      snoozedAt: value.snoozedAt,
    };
  }
  return undefined;
}

function readAskUserQuestionTexts(input: Record<string, unknown>): string[] {
  const questions = input.questions;
  if (!Array.isArray(questions)) {
    return [];
  }
  return questions
    .map((question) =>
      isRecord(question) && typeof question.question === "string" ? question.question : undefined,
    )
    .filter((question): question is string => question !== undefined);
}

function normalizeAnswerValue(value: unknown): string | undefined {
  if (typeof value === "string") {
// The old client used an empty string to indicate skipping; discard blank to prevent it from entering
// answers are later treated as user preferences by core. Non-null answers also strip peripheral whitespace at protocol boundaries.
    const trimmed = value.trim();
    return trimmed.length > 0 ? trimmed : undefined;
  }
  if (Array.isArray(value)) {
    return value
      .map((item) => (typeof item === "string" ? item.trim() : ""))
      .filter((item) => item.length > 0)
      .join(", ");
  }
  return undefined;
}

function normalizeAskUserQuestionAnnotations(
  value: unknown,
): Record<string, { preview?: string; notes?: string }> | undefined {
  if (!isRecord(value)) {
    return undefined;
  }

  const entries = Object.entries(value)
    .map(([question, annotation]) => {
      if (!isRecord(annotation)) {
        return undefined;
      }
      const normalizedAnnotation = {
        ...(typeof annotation.preview === "string" ? { preview: annotation.preview } : {}),
        ...(typeof annotation.notes === "string" ? { notes: annotation.notes } : {}),
      };
      return Object.keys(normalizedAnnotation).length > 0
        ? ([question, normalizedAnnotation] as const)
        : undefined;
    })
    .filter(
      (entry): entry is readonly [string, { preview?: string; notes?: string }] =>
        entry !== undefined,
    );

  return entries.length > 0 ? Object.fromEntries(entries) : undefined;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
