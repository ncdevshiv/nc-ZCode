import type { ZCodeMessagePart, ZCodeMessageWithParts, ZCodeToolState } from "@zcode/shared";
import type { MessagePart, MessageWithParts, ToolState } from "@zcode/contracts";
import { shouldHideInvalidToolCallFromProduct } from "../tool-call-product-visibility.js";

export function mapMessageWithParts(message: MessageWithParts): ZCodeMessageWithParts {
  return {
    info:
      message.info.role === "user"
        ? {
            agent: message.info.agent,
            messageId: String(message.info.id),
            model: message.info.modelSelection,
            metadata: message.info.metadata,
            role: "user",
            semantics: message.info.semantics,
            sessionId: String(message.info.sessionID),
            source: message.info.source,
            system: message.info.system,
            synthetic: message.info.synthetic,
            time: message.info.time,
            tools: message.info.tools,
            visibility: message.info.visibility,
          }
        : {
            agent: message.info.agent,
            cost: message.info.cost,
            error: message.info.error
              ? ({ name: message.info.error.name, data: message.info.error.data } as Record<
                  string,
                  unknown
                >)
              : undefined,
            finish: message.info.finish,
            messageId: String(message.info.id),
            model:
              message.info.providerId && message.info.modelId
                ? {
                    providerId: message.info.providerId,
                    modelId: message.info.modelId,
                    ...(message.info.reasoningLevel
                      ? { options: { reasoningLevel: message.info.reasoningLevel } }
                      : {}),
                  }
                : undefined,
            parentMessageId: String(message.info.parentID),
            path: message.info.path,
            role: "assistant",
            semantics: message.info.semantics,
            sessionId: String(message.info.sessionID),
            structured: message.info.structured,
            time: message.info.time,
            tokens: message.info.tokens,
          },
    parts: message.parts
      .filter(
        (part) =>
          part.type !== "tool" || !shouldHideInvalidToolCallFromProduct(part.tool, part.metadata),
      )
      .map(mapMessagePart),
  };
}

function mapMessagePart(part: MessagePart): ZCodeMessagePart {
  const base = {
    messageId: String(part.messageID),
    partId: String(part.id),
    sessionId: String(part.sessionID),
  };
  switch (part.type) {
    case "text":
      return {
        ...base,
        ignored: part.ignored,
        metadata: part.metadata,
        synthetic: part.synthetic,
        text: part.text,
        type: "text",
      };
    case "reasoning":
      return { ...base, metadata: part.metadata, text: part.text, type: "reasoning" };
    case "file":
      return {
        ...base,
        filename: part.filename,
        metadata: part.metadata as Record<string, unknown> | undefined,
        mime: part.mime,
        type: "file",
        url: part.url,
      };
    case "tool":
      return {
        ...base,
        callId: part.callID,
        metadata: mapToolPartMetadata(part.metadata),
        state: mapToolState(part.state),
        tool: part.tool,
        type: "tool",
      };
    case "step-start":
      return { ...base, snapshot: part.snapshot, type: "step-start" };
    case "step-finish":
      return {
        ...base,
        cost: part.cost,
        reason: part.reason,
        snapshot: part.snapshot,
        tokens: part.tokens,
        type: "step-finish",
      };
    case "snapshot":
      return { ...base, snapshot: part.snapshot, type: "snapshot" };
    case "patch":
      return { ...base, files: part.files, hash: part.hash, type: "patch" };
    case "compaction":
      return {
        ...base,
        auto: part.auto,
        metadata: {
          attempt: part.attempt,
          boundaryId: part.boundaryId,
          compactReason: part.compactReason,
          endedAt: part.time?.end,
          maxAttempts: part.maxAttempts,
          operationId: part.operationId,
          phase: part.phase,
          postCompactTokenCount: part.postCompactTokenCount,
          preCompactTokenCount: part.preCompactTokenCount,
          reason: part.reason,
          replace: part.replace,
          startedAt: part.time?.start,
          summaryMessageId: part.summaryMessageId,
          timelineStatus: part.timelineStatus,
          truePostCompactTokenCount: part.truePostCompactTokenCount,
          trigger: part.trigger,
        },
        reason: part.reason,
        summaryMessageId: part.summaryMessageId,
        type: "compaction",
      };
    case "timeline":
      return {
        ...base,
        anchorMessageId: part.anchorMessageId ? String(part.anchorMessageId) : undefined,
        anchorTurnId: part.anchorTurnId ? String(part.anchorTurnId) : undefined,
        attempt: part.timelineType === "context_compaction" ? part.attempt : undefined,
        boundaryId: part.timelineType === "context_compaction" ? part.boundaryId : undefined,
        compactReason: part.timelineType === "context_compaction" ? part.compactReason : undefined,
        display: part.display,
        fromModel: part.timelineType === "model_change" ? part.fromModel : undefined,
        goalIteration: part.timelineType === "goal_verification" ? part.goalIteration : undefined,
        maxAttempts: part.timelineType === "context_compaction" ? part.maxAttempts : undefined,
        operationId: part.timelineType === "context_compaction" ? part.operationId : undefined,
        parentSessionId:
          part.timelineType === "session_fork" ? String(part.parentSessionId) : undefined,
        phase: part.timelineType === "context_compaction" ? part.phase : undefined,
        postCompactTokenCount:
          part.timelineType === "context_compaction" ? part.postCompactTokenCount : undefined,
        preCompactTokenCount:
          part.timelineType === "context_compaction" ? part.preCompactTokenCount : undefined,
        reason: part.timelineType === "context_compaction" ? part.reason : undefined,
        restoredFileCount:
          part.timelineType === "session_fork" ? part.restoredFileCount : undefined,
        status: part.status,
        summaryMessageId:
          part.timelineType === "context_compaction" && part.summaryMessageId
            ? String(part.summaryMessageId)
            : undefined,
        targetCheckpointId:
          part.timelineType === "session_fork" ? part.targetCheckpointId : undefined,
        targetId: part.timelineType === "goal_verification" ? part.targetId : undefined,
        targetMessageId:
          part.timelineType === "session_fork" ? String(part.targetMessageId) : undefined,
        time: part.time,
        timelineType: part.timelineType,
        toModel: part.timelineType === "model_change" ? part.toModel : undefined,
        trigger: part.timelineType === "context_compaction" ? part.trigger : undefined,
        truePostCompactTokenCount:
          part.timelineType === "context_compaction" ? part.truePostCompactTokenCount : undefined,
        type: "timeline",
        verification: part.timelineType === "goal_verification" ? part.verification : undefined,
        verificationId: part.timelineType === "goal_verification" ? part.verificationId : undefined,
      };
    case "subtask":
      return {
        ...base,
        agent: part.agent,
        command: part.command,
        description: part.description,
        model: part.model,
        prompt: part.prompt,
        type: "subagent",
      };
    case "agent":
      return { ...base, name: part.name, type: "agent" };
    case "retry":
      return {
        ...base,
        attempt: part.attempt,
        error: { name: part.error.name, data: part.error.data },
        type: "retry",
      };
  }
}

function mapToolPartMetadata(
  metadata: Record<string, unknown> | undefined,
): Record<string, unknown> | undefined {
  if (!metadata || !Object.prototype.hasOwnProperty.call(metadata, "providerToolName")) {
    return metadata;
  }
// providerToolName is only used for Agent cold hydration to restore the original empty name of the model; App uses
// The non-empty placeholder value of ToolPart.tool cannot project the internal provider field to the public protocol.
  const visibleMetadata = { ...metadata };
  delete visibleMetadata.providerToolName;
  return Object.keys(visibleMetadata).length > 0 ? visibleMetadata : undefined;
}

function mapToolState(state: ToolState): ZCodeToolState {
  switch (state.status) {
    case "pending":
      return { input: state.input, raw: state.raw, status: "pending" };
    case "running":
      return {
        input: state.input,
        metadata: mapToolStateMetadata(state.metadata),
        startedAt: state.time.start,
        status: "running",
        title: state.title,
      };
    case "completed":
      return {
        completedAt: state.time.end,
        input: state.input,
        metadata: mapCompletedToolStateMetadata(state.metadata),
        output: state.output,
        startedAt: state.time.start,
        status: "completed",
        title: state.title,
      };
    case "error":
      return {
        completedAt: state.time.end,
        error: state.error,
        input: state.input,
        metadata: mapErrorToolStateMetadata(state.metadata),
        startedAt: state.time.start,
        status: "error",
      };
  }
}

function mapToolStateMetadata(
  metadata: Record<string, unknown> | undefined,
): Record<string, unknown> | undefined {
  if (!metadata || !Object.prototype.hasOwnProperty.call(metadata, "readFileState")) {
    return metadata;
  }
  const protocolMetadata = { ...metadata };
// readFileState carries a complete file snapshot and is only used for agent resume internal recovery;
// The app/remote protocol only needs to display metadata, and cannot hide the file content in tool state metadata.
  delete protocolMetadata.readFileState;
  return protocolMetadata;
}

function mapErrorToolStateMetadata(
  metadata: Record<string, unknown> | undefined,
): Record<string, unknown> | undefined {
  const protocolMetadata = mapToolStateMetadata(metadata);
  if (
    !protocolMetadata ||
    !Object.prototype.hasOwnProperty.call(protocolMetadata, "modelContent")
  ) {
    return protocolMetadata;
  }
// error modelContent is only used for Agent to accurately restore provider history,
// Cannot be exposed as new App / remote protocol metadata.
  const visibleMetadata = { ...protocolMetadata };
  delete visibleMetadata.modelContent;
  return visibleMetadata;
}

function mapCompletedToolStateMetadata(
  metadata: Record<string, unknown> | undefined,
): Record<string, unknown> {
  const protocolMetadata = mapToolStateMetadata(metadata);
  if (
    !protocolMetadata ||
    !Object.prototype.hasOwnProperty.call(protocolMetadata, "modelContentLayout")
  ) {
    return protocolMetadata ?? {};
  }
// modelContentLayout is only used for Agent cold recovery and reconstruction tool result media sequence,
// Completed metadata mapping as is will bring internal layout and repeated text into the App / remote replayable payload.
  const visibleMetadata = { ...protocolMetadata };
  delete visibleMetadata.modelContentLayout;
// core's history/exception completed tool part may not have metadata, but
// The completed tool state of ZCode protocol requires that metadata must be object.
  return visibleMetadata;
}
