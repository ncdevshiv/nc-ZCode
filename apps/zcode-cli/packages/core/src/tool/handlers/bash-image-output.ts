import { readFile, stat } from "node:fs/promises";
import {
  READ_IMAGE_MAX_DIMENSION,
  detectImageMediaType,
  isImageProcessorPortError,
  parseImageDataUrl,
  type ParsedImageDataUrl,
  type TraceContext,
} from "@zcode/contracts";
import type { ToolExecutionContext } from "../types.js";

const MAX_IMAGE_FILE_BYTES = 20 * 1024 * 1024;

interface BashImageSource {
  artifactPath?: string;
  artifactSize?: number;
  inline: string;
}

export async function prepareBashImageOutput(
  stdout: BashImageSource,
  context: ToolExecutionContext,
): Promise<{ stdout: string } | undefined> {
  const source = await readBashImageSource(stdout);
  const parsed = parseImageDataUrl(source);
  if (!parsed) return undefined;
  if (!context.imageProcessorPort) return { stdout: parsed.dataUrl };

  try {
    const resized = await context.imageProcessorPort.resizeToFit(
      {
        data: parsed.data,
        maxDimension: READ_IMAGE_MAX_DIMENSION,
        mediaType: parsed.mediaType,
        trace: createToolTrace(context),
      },
      { signal: context.abortSignal },
    );
    return {
      stdout: `data:${resized.mediaType};base64,${Buffer.from(resized.data).toString("base64")}`,
    };
  } catch (error) {
    if (context.abortSignal.aborted) throw error;
    if (shouldFallbackToOriginalImage(error)) {
      const fallback = fallbackValidImageDataUrl(parsed);
      if (fallback) return fallback;
    }
// Bash image output is a best-effort capability; when invalid image decoding fails, the text is rolled back to avoid sending bad image blocks to the provider.
    return undefined;
  }
}

async function readBashImageSource(stdout: BashImageSource): Promise<string> {
  if (!stdout.artifactPath) return stdout.inline;

  try {
    const size = stdout.artifactSize ?? (await stat(stdout.artifactPath)).size;
    if (size > MAX_IMAGE_FILE_BYTES) return stdout.inline;
    return await readFile(stdout.artifactPath, "utf8");
  } catch {
// Image recognition is provider-visible enhancement; fallback to inline output when artifact temporary file is missing, retaining Bash original results.
    return stdout.inline;
  }
}

function fallbackValidImageDataUrl(input: ParsedImageDataUrl): { stdout: string } | undefined {
  const detected = detectImageMediaType(input.data);
  if (!detected || detected !== input.mediaType) return undefined;
// When the image stdout has passed magic verification, resize is only model budget optimization.
// The image block visible to the model cannot be downgraded to the original data URI text due to optimization failure.
  return { stdout: input.dataUrl };
}

function shouldFallbackToOriginalImage(error: unknown): boolean {
  if (!isImageProcessorPortError(error)) return true;
  return error.code === "processing_failed";
}

function createToolTrace(context: ToolExecutionContext): TraceContext {
  return {
    traceId: context.traceId,
    spanId: context.spanId,
    parentSpanId: context.parentSpanId,
    sessionId: context.sessionId,
    turnId: context.turnId,
    attributes: {
      toolCallId: context.toolCallId,
      toolName: "Bash",
    },
  } as unknown as TraceContext;
}
