// Video branch of the Read tool: no transcoding/compression dependencies (CLI does not include ffmpeg), read-only base64 + size check.
// On the wire side, the adapter converts the video block into video_url / anthropic video block (see transform.ts and two AI SDK patches).
import {
  CoreErrorType,
  READ_VIDEO_MAX_INPUT_BYTES,
  createCoreError,
  isFileSystemPortError,
  type ReadVideoOutput,
  type TraceContext,
} from "@zcode/contracts";
import type { ToolExecutionContext } from "../types.js";
import type { VideoInputMimeType } from "../../runtime/helpers/attachment-video.js";

export async function readVideoFile(
  filePath: string,
  mimeType: VideoInputMimeType,
  context: ToolExecutionContext,
): Promise<ReadVideoOutput> {
  const fileSystemPort = context.fileSystemPort;
  if (!fileSystemPort) {
    throw createCoreError(
      CoreErrorType.ConfigurationError,
      "FileSystemPort is not configured for Read tool",
      {
        context: {
          toolCallId: context.toolCallId,
          toolName: "Read",
        },
        recoverable: false,
      },
    );
  }

  const trace = createToolTrace(context);
  try {
    const read = await fileSystemPort.readBinaryFile(
      {
        path: filePath,
        maxBytes: READ_VIDEO_MAX_INPUT_BYTES,
        trace,
      },
      { signal: context.abortSignal },
    );
    if (read.bytesRead === 0) {
// Empty video will generate an empty data URL, which will then be discarded by the adapter, but Read has been declared successful.
      throw createCoreError(CoreErrorType.ToolExecutionFailed, "Cannot read an empty video file.", {
        context: {
          code: "read_video_input_empty",
          filePath,
          toolCallId: context.toolCallId,
          toolName: "Read",
        },
        recoverable: true,
      });
    }
    return {
      type: "video",
      base64: Buffer.from(read.content).toString("base64"),
      mimeType,
      originalSize: read.sizeBytes,
    };
  } catch (error) {
    if (isFileSystemPortError(error) && error.code === "too_large") {
      throw createCoreError(CoreErrorType.ToolExecutionFailed, error.message, {
        cause: error,
        context: {
          code: "read_video_input_too_large",
          filePath,
          maxBytes: READ_VIDEO_MAX_INPUT_BYTES,
          toolCallId: context.toolCallId,
          toolName: "Read",
        },
        recoverable: true,
      });
    }
    throw error;
  }
}

function createToolTrace(context: ToolExecutionContext): TraceContext {
  return {
    traceId: context.traceId,
    spanId: context.spanId,
    parentSpanId: context.parentSpanId,
    sessionId: context.sessionId,
    turnId: context.turnId,
  } as unknown as TraceContext;
}
