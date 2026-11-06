// SessionEvent payload → Constructor pure function of ConversationRow.
// row self-contained principle: Each row produced here must be able to be rendered without relying on other rows.
import type {
  CompactTimelineStatus,
  CompactTrigger,
  GoalStatus,
  SyntheticUserMessageSource,
  ToolResultPayload,
  TurnResultType,
  TurnStartedPayload,
} from "@zcode/contracts";
import type {
  GoalState,
  TimelineMarkerPayload,
  ToolOutput,
  TurnHeaderRow,
  UserInputRow,
} from "@zcode/shared/zcode-protocol-v4";
import { PROTOCOL_V4_LIMITS } from "@zcode/shared/zcode-protocol-v4";

interface RowBaseInput {
  rowId: number;
  turnId: string;
  createdAt: number;
  createdAtSeq: number;
}

// inputSource → turnHeader.origin。
function mapTurnHeaderOrigin(
  source: SyntheticUserMessageSource | undefined,
): TurnHeaderRow["origin"] {
  switch (source) {
    case "background_task":
      return "backgroundResult";
    case "goal-continuation":
      return "goalContinuation";
    case "rewind":
      return "editRerun";
    default:
      return "userInput";
  }
}

// inputSource → userInput.origin。
function mapUserInputOrigin(
  source: SyntheticUserMessageSource | undefined,
): UserInputRow["origin"] {
  switch (source) {
    case "background_task":
      return "backgroundResult";
    case "goal-continuation":
      return "goalContinuation";
    case "subagent":
    case "subagent_message":
      return "mailbox";
    case "fork":
    case "plugin_reference":
    case "rewind":
    case "todo_reminder":
      return "synthetic";
    default:
      return "realUser";
  }
}

export function buildTurnHeaderRow(base: RowBaseInput, payload: TurnStartedPayload): TurnHeaderRow {
  return {
    ...base,
    kind: "turnHeader",
    origin: mapTurnHeaderOrigin(payload.inputSource),
    executionKind: payload.executionKind ?? "agent",
    ...(payload.originMeta ? { originMeta: payload.originMeta } : {}),
    state: "running",
    startedAt: base.createdAt,
  };
}

export function buildUserInputRow(base: RowBaseInput, payload: TurnStartedPayload): UserInputRow {
// Attachment rendering: Display meta information carried by TurnStarted → row.attachments.
// ref is the content reference placeholder (local path/artifact URI); when there is no stable reference, the inline serial number is used as the placeholder.
// The display layer only uses fileName/mime/bytes and does not retrieve content based on ref (attachment/get query is a follow-up).
  const attachments =
    payload.intent?.attachmentRefs ??
    payload.attachments?.map((meta, index) => ({
      ref: meta.ref ?? `turn-attachment/${base.rowId}/${index}`,
      fileName: meta.fileName,
      mime: meta.mime,
      bytes: meta.bytes,
    }));
  const sourceCommandId = payload.intent?.sourceCommandId ?? payload.inputId;
  const rootSourceCommandId = payload.intent?.provenance?.sourceCommandId ?? sourceCommandId;
  return {
    ...base,
    kind: "userInput",
    text: payload.input,
    origin: mapUserInputOrigin(payload.inputSource),
    ...(sourceCommandId ? { sourceCommandId } : {}),
    ...(rootSourceCommandId ? { rootSourceCommandId } : {}),
    ...(payload.intent?.clientId ? { clientId: payload.intent.clientId } : {}),
    ...(payload.epilogueStart === undefined ? {} : { epilogueStart: payload.epilogueStart }),
    ...(attachments && attachments.length > 0 ? { attachments } : {}),
  };
}

export function mapTurnResultToHeaderState(
  resultType: TurnResultType,
): Exclude<TurnHeaderRow["state"], "running"> {
  switch (resultType) {
    case "success":
      return "completedSuccess";
    case "cancelled":
      return "completedInterrupted";
    default:
      return "failed";
  }
}

// CompactTimelineStatus → compact marker.status。
// Semantic mapping: retrying is still running; skipped = nothing happened (noop); interrupted = stopped (cancelled).
export function mapCompactMarkerStatus(
  status: CompactTimelineStatus,
): Extract<TimelineMarkerPayload, { type: "compact" }>["status"] {
  switch (status) {
    case "started":
    case "retrying":
      return "running";
    case "completed":
      return "success";
    case "skipped":
      return "noop";
    case "interrupted":
      return "cancelled";
    default:
      return "failed";
  }
}

// CompactTrigger → marker.origin: outside manual (auto/partial/reactive/session_memory)
// All return to auto - the UI only distinguishes between "user clicked" and "system triggered".
export function mapCompactMarkerOrigin(
  trigger: CompactTrigger,
): Extract<TimelineMarkerPayload, { type: "compact" }>["origin"] {
  return trigger === "manual" ? "manual" : "auto";
}

// old GoalStatus → v4 GoalState.status.
// budget_limited returns to paused: the budget is exhausted and is stopped, waiting for the user to explicitly resume;
// complete returns to verified: the old vocabulary list does not have verifying/notSatisfied subdivisions, and the final state is semantically equivalent.
export function mapGoalStatus(status: GoalStatus): GoalState["status"] {
  switch (status) {
    case "active":
      return "active";
    case "complete":
      return "verified";
    default:
      return "paused";
  }
}

//The final state is truncated and all files are unified: head+tail are 32K each, and the excess is pulled on demand using truncated.ref.
// Stage ref first uses toolCallId to occupy the space (artifact access belongs to the transmission shell stage).
export function buildToolOutput(result: ToolResultPayload, toolCallId: string): ToolOutput {
  const text = result.content;
// The visible text of the model only retains the image placeholder. If the V4 output does not carry display independently,
// Both real-time projection and cold recovery will lose CUA screenshots. Node REPL images still use the ToolCallRow.display dedicated channel.
  const display = result.display?.kind === "node_repl_images" ? undefined : result.display;
  const headBytes = PROTOCOL_V4_LIMITS.toolOutputFinalHeadBytes;
  const tailBytes = PROTOCOL_V4_LIMITS.toolOutputFinalTailBytes;
  const totalBytes = Buffer.byteLength(text, "utf8");
  if (totalBytes <= headBytes + tailBytes) {
    return { text, ...(display ? { display } : {}) };
  }
  const buffer = Buffer.from(text, "utf8");
  const head = buffer.subarray(0, headBytes).toString("utf8");
  const tail = buffer.subarray(buffer.length - tailBytes).toString("utf8");
  return {
    text: `${head}\n…\n${tail}`,
    ...(display ? { display } : {}),
    truncated: { totalBytes, ref: `tool-output/${toolCallId}` },
  };
}
