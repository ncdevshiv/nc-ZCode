// v4 command native executor.
//
// How to advance command-by-command nativeization: handlers/ registry lists native commands; binder only supports
// When hit, go to this executor, if not hit, fall back to the old bridge (fall back on the binder side, and disappear one by one with the nativeization of each command).
//Complete definition: native handler + L2 closed loop + L3 e2e, and the write path does not pass through the old protocol code.
import type { CommandEnvelope, CommandResult } from "@zcode/shared/zcode-protocol-v4";
import { NATIVE_HANDLERS } from "./handlers/index.js";
import type { V4CommandCoreHost } from "./types.js";

const SELECTION_SIDE_CHAT_RESTRICTED_COMMANDS = new Set<CommandEnvelope["type"]>([
  "sendGoalCommand",
  "pauseGoal",
  "resumeGoal",
  "editUserQuery",
  "retryTurn",
  "forkAssistant",
  "discardSharedContext",
]);

class V4SelectionSideChatRestrictedCommandError extends Error {
  readonly reasonCode = "guard.selectionSideChatRestrictedCommand";

  constructor(command: CommandEnvelope["type"]) {
    super(`selection_side_chat is not allowed to execute ${command}`);
    this.name = "V4SelectionSideChatRestrictedCommandError";
  }
}

export { V4SessionNotFoundError } from "./record-access.js";

export class V4CommandExecutor {
  constructor(private readonly host: V4CommandCoreHost) {}

/** The native command set (binder is diverted accordingly; the old bridge is deleted as a whole after all hits). */
  supports(type: CommandEnvelope["type"]): boolean {
    return type in NATIVE_HANDLERS;
  }

  async execute(
    envelope: CommandEnvelope,
    admission?: V4CommandAdmission,
    executionContext?: V4CommandExecutionContext,
  ): Promise<CommandResult | undefined> {
    if (
      envelope.sessionId &&
      SELECTION_SIDE_CHAT_RESTRICTED_COMMANDS.has(envelope.type) &&
      this.host.getRecord(envelope.sessionId)?.taskType === "selection_side_chat"
    ) {
      throw new V4SelectionSideChatRestrictedCommandError(envelope.type);
    }
    const handler = NATIVE_HANDLERS[envelope.type as keyof typeof NATIVE_HANDLERS];
    if (!handler) {
      throw new Error(`v4 native executor does not support: ${envelope.type}`);
    }
    return handler(this.host, {
      ...envelope,
      ...(admission ? { __v4Admission: admission } : {}),
      ...(executionContext ? { __v4ExecutionContext: executionContext } : {}),
    });
  }
}

interface V4CommandAdmission {
  admissionSeq: number;
  admittedAt: number;
  queueItemId: string;
}

interface V4CommandExecutionContext {
/** Internal auto-drain must be atomically verified by the handler before reserve. Core idle. */
  autoDrainPromotion?: true;
}

type V4AdmittedCommandEnvelope = CommandEnvelope & {
  __v4Admission?: V4CommandAdmission;
  __v4ExecutionContext?: V4CommandExecutionContext;
};

export function commandAdmissionOf(envelope: CommandEnvelope): V4CommandAdmission {
  return (
    (envelope as V4AdmittedCommandEnvelope).__v4Admission ?? {
      admissionSeq: 0,
      admittedAt: Date.now(),
      queueItemId: `queue_${envelope.commandId}`,
    }
  );
}

export function commandExecutionContextOf(
  envelope: CommandEnvelope,
): V4CommandExecutionContext | undefined {
  return (envelope as V4AdmittedCommandEnvelope).__v4ExecutionContext;
}
