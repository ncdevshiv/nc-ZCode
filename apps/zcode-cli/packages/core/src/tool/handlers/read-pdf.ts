import { basename } from "node:path";
import {
  CoreErrorType,
  READ_IMAGE_MAX_BASE64_BYTES,
  READ_IMAGE_MAX_DIMENSION,
  READ_IMAGE_TARGET_BYTES,
  READ_IMAGE_TOKEN_TO_BASE64_CHAR_RATIO,
  READ_MAX_OUTPUT_TOKENS,
  READ_PDF_EXTRACT_MAX_INPUT_BYTES,
  READ_PDF_MAX_PAGES_PER_REQUEST,
  READ_PDF_NATIVE_MAX_INPUT_BYTES,
  READ_PDF_NATIVE_MAX_PAGES,
  READ_PDF_RENDER_TIMEOUT_MS,
  ReadErrorCode,
  ReadInputJsonSchema,
  ReadPdfInputJsonSchema,
  PdfDocumentPortError,
  createCoreError,
  parseReadPdfPageRange,
  type JsonSchema,
  type ModelMessageContent,
  type PdfDocumentErrorCode,
  type ReadImageOutput,
  type ReadInput,
  type ReadOutput,
  type ReadPartsOutput,
  type ReadPdfOutput,
  type TraceContext,
} from "@zcode/contracts";
import type {
  ToolExecutionContext,
  ToolExecutionModelContext,
  ToolHandlerFailure,
} from "../types.js";

const READ_PROVIDER_PDF_DESCRIPTION_LINE =
  '- Reads PDFs via the `pages` parameter (e.g. "1-5", max 20 pages/request; required for PDFs over 10 pages).';
const READ_ERROR_CODE_BY_PDF_DOCUMENT_ERROR = {
  cancelled: undefined,
  corrupted: ReadErrorCode.PDF_INVALID,
  io_error: ReadErrorCode.PDF_IO_ERROR,
  page_out_of_range: ReadErrorCode.PDF_PAGE_OUT_OF_RANGE,
  password_protected: ReadErrorCode.PDF_PASSWORD_PROTECTED,
  permission_denied: ReadErrorCode.PDF_PERMISSION_DENIED,
  process_failed: ReadErrorCode.PDF_PROCESS_FAILED,
  timeout: ReadErrorCode.PDF_TIMEOUT,
  unavailable: ReadErrorCode.PDF_CONFIGURATION_ERROR,
} satisfies Record<PdfDocumentErrorCode, ReadErrorCode | undefined>;
// Root cause: 120 seconds only belongs to the Poppler sub-process; if the executor uses the same upper limit, it will be occupied in advance
// Time for availability checks, image normalization and finally cleanup, so the pages branch retains a separate outer budget.
export const READ_PDF_TOOL_TIMEOUT_MS = READ_PDF_RENDER_TIMEOUT_MS + 30_000;

export function isPdfPath(filePath: string): boolean {
  return filePath.toLowerCase().endsWith(".pdf");
}

export function supportsPdfForExecution(context: ToolExecutionModelContext): boolean {
  return context.model?.properties.inputFormat.supportsPdf === true;
}

function supportsImagesForExecution(context: ToolExecutionModelContext): boolean | undefined {
  return context.model?.properties.inputFormat.supportsImage;
}

export function resolveReadInputSchema(context: ToolExecutionModelContext): JsonSchema {
  return supportsPdfForExecution(context) ? ReadPdfInputJsonSchema : ReadInputJsonSchema;
}

export function resolveReadProviderDescription(
  baseDescription: string,
  context: ToolExecutionModelContext,
): string {
  if (!supportsPdfForExecution(context)) return baseDescription;
  const lines = baseDescription.split("\n");
  const imageLineIndex = lines.findIndex((line) => line.startsWith("- Reads images"));
  if (imageLineIndex < 0) return `${baseDescription}\n${READ_PROVIDER_PDF_DESCRIPTION_LINE}`;
  lines.splice(imageLineIndex + 1, 0, READ_PROVIDER_PDF_DESCRIPTION_LINE);
  return lines.join("\n");
}

export function resolveReadTimeoutBudgetMs(
  input: unknown,
  context?: ToolExecutionModelContext,
): number | undefined {
  if (!context || !supportsPdfForExecution(context) || !isRecord(input)) return undefined;
  return typeof input.file_path === "string" &&
    isPdfPath(input.file_path) &&
    typeof input.pages === "string"
    ? READ_PDF_TOOL_TIMEOUT_MS
    : undefined;
}

export async function readPdfFile(
  input: Pick<ReadInput, "pages"> & { filePath: string },
  context: ToolExecutionContext,
): Promise<ReadOutput | ToolHandlerFailure> {
  if (input.pages !== undefined && supportsImagesForExecution(context) === false) {
    return failure(
      ReadErrorCode.PDF_PAGES_IMAGES_UNSUPPORTED,
      "The current model supports PDF input but does not support image input; remove the pages parameter.",
    );
  }
  const fileSystemPort = context.fileSystemPort;
  if (!fileSystemPort) {
    return failure(
      ReadErrorCode.PDF_CONFIGURATION_ERROR,
      "FileSystemPort is not configured for Read tool.",
    );
  }
  const trace = createToolTrace(context);
  const stat = await fileSystemPort.stat(
    { path: input.filePath, trace },
    { signal: context.abortSignal },
  );
  if (stat.kind !== "file") {
    return failure(ReadErrorCode.PDF_INVALID, `Path is not a regular file: ${input.filePath}`);
  }
  if (stat.sizeBytes === 0) {
    return failure(ReadErrorCode.PDF_INVALID, `PDF file is empty: ${input.filePath}`);
  }

  return input.pages === undefined
    ? await readNativePdf(input.filePath, stat.sizeBytes, trace, context)
    : await readPdfPages(input.filePath, input.pages, stat.sizeBytes, trace, context);
}

async function readNativePdf(
  filePath: string,
  sizeBytes: number,
  trace: TraceContext,
  context: ToolExecutionContext,
): Promise<ReadPdfOutput | ToolHandlerFailure> {
  if (sizeBytes > READ_PDF_NATIVE_MAX_INPUT_BYTES) {
    return failure(
      ReadErrorCode.PDF_TOO_LARGE,
      `PDF file exceeds maximum allowed size of ${formatFileSize(READ_PDF_NATIVE_MAX_INPUT_BYTES)}.`,
    );
  }

  if (context.pdfDocumentPort) {
    try {
      const pageCount = await context.pdfDocumentPort.getPageCount(
        { filePath, trace },
        { signal: context.abortSignal },
      );
      if (pageCount !== undefined && pageCount > READ_PDF_NATIVE_MAX_PAGES) {
        return failure(
          ReadErrorCode.PDF_TOO_MANY_PAGES,
          `This PDF has ${pageCount} pages, which is too many to read at once. Use the pages parameter to read specific page ranges (e.g., pages: "1-5"). Maximum ${READ_PDF_MAX_PAGES_PER_REQUEST} pages per request.`,
        );
      }
    } catch (error) {
      rethrowPdfCancellation(error, context.abortSignal);
// Root cause: The adapter has used undefined to express the normal execution or parsing failure of pdfinfo; continue to swallow
// Unknown exceptions thrown by the port can mask implementation failures. Here only the conversion is canceled, the rest of the exceptions keep their causes and bubble up.
      throw error;
    }
  }

  const read = await context.fileSystemPort!.readBinaryFile(
    { path: filePath, maxBytes: READ_PDF_NATIVE_MAX_INPUT_BYTES, trace },
    { signal: context.abortSignal },
  );
  if (!hasPdfMagic(read.content)) {
    return failure(
      ReadErrorCode.PDF_INVALID,
      `File is not a valid PDF (missing %PDF- header): ${filePath}`,
    );
  }
  return {
    type: "pdf",
    filePath,
    base64: Buffer.from(read.content).toString("base64"),
    originalSize: read.sizeBytes,
  };
}

async function readPdfPages(
  filePath: string,
  pagesInput: string,
  sizeBytes: number,
  trace: TraceContext,
  context: ToolExecutionContext,
): Promise<ReadPartsOutput | ToolHandlerFailure> {
  if (sizeBytes > READ_PDF_EXTRACT_MAX_INPUT_BYTES) {
    return failure(
      ReadErrorCode.PDF_TOO_LARGE,
      `PDF file exceeds maximum allowed size for text extraction (${formatFileSize(READ_PDF_EXTRACT_MAX_INPUT_BYTES)}).`,
    );
  }
  const range = parseReadPdfPageRange(pagesInput);
  if (!range) {
    return failure(
      ReadErrorCode.PDF_INVALID,
      `Invalid pages parameter: "${pagesInput}". Use formats like "1-5", "3", or "10-20". Pages are 1-indexed.`,
    );
  }
  if (
    range.lastPage === Number.POSITIVE_INFINITY ||
    range.lastPage - range.firstPage + 1 > READ_PDF_MAX_PAGES_PER_REQUEST
  ) {
    return failure(
      ReadErrorCode.PDF_INVALID,
      `Page range "${pagesInput}" exceeds maximum of ${READ_PDF_MAX_PAGES_PER_REQUEST} pages per request. Please use a smaller range.`,
    );
  }
  if (!context.pdfDocumentPort) {
    return failure(
      ReadErrorCode.PDF_CONFIGURATION_ERROR,
      "PDF page extraction is not configured in this runtime.",
    );
  }
  if (!context.imageProcessorPort) {
    return failure(
      ReadErrorCode.PDF_CONFIGURATION_ERROR,
      "ImageProcessorPort is not configured for PDF page extraction.",
    );
  }

  try {
    const renderedPages = await context.pdfDocumentPort.renderPages(
      {
        filePath,
        firstPage: range.firstPage,
        lastPage: range.lastPage,
        trace,
      },
      { signal: context.abortSignal },
    );
    const preparedPages = await Promise.all(
      renderedPages.map(async (page) => {
        const prepared = await context.imageProcessorPort!.prepareForModel(
          {
            data: page.data,
            maxBase64Bytes: READ_IMAGE_MAX_BASE64_BYTES,
            maxDimension: READ_IMAGE_MAX_DIMENSION,
            maxRawBytes: READ_IMAGE_TARGET_BYTES,
            maxTokens: READ_MAX_OUTPUT_TOKENS,
            mediaType: page.mediaType,
            tokenToBase64CharRatio: READ_IMAGE_TOKEN_TO_BASE64_CHAR_RATIO,
            trace,
          },
          { signal: context.abortSignal },
        );
        return {
          type: "image" as const,
          pageNumber: page.pageNumber,
          base64: Buffer.from(prepared.data).toString("base64"),
          mimeType: isReadImageMime(prepared.mediaType) ? prepared.mediaType : page.mediaType,
          originalSize: page.data.byteLength,
          transformedSize: prepared.transformedSizeBytes,
          resized: prepared.resized,
          compressed: prepared.compressed,
          compressionStrategy: prepared.strategy,
          dimensions: {
            originalWidth: prepared.originalWidth,
            originalHeight: prepared.originalHeight,
            displayWidth: prepared.width,
            displayHeight: prepared.height,
          },
        } satisfies ReadImageOutput & { pageNumber: number };
      }),
    );
    preparedPages.sort((left, right) => left.pageNumber - right.pageNumber);
    return {
      type: "parts",
      filePath,
      numParts: preparedPages.length,
      originalSize: sizeBytes,
      pages: preparedPages,
    };
  } catch (error) {
    rethrowPdfCancellation(error, context.abortSignal);
    if (error instanceof PdfDocumentPortError) {
      const errorCode = READ_ERROR_CODE_BY_PDF_DOCUMENT_ERROR[error.code];
      if (errorCode === undefined) throw error;
// Root cause: The adapter has provided a stable error category, which will be folded into PDF_INVALID to make the executor,
// telemetry and the caller cannot distinguish environment faults from input errors; core only does exhaustive mapping and does not parse copywriting.
      return failure(errorCode, error.message);
    }
    throw error;
  }
}

function rethrowPdfCancellation(error: unknown, signal: AbortSignal): void {
// Root cause: ExecutionPort can return canceled during runtime shutdown without changing the caller's
// AbortSignal. If the port error bubbles up directly, the executor will classify it as an ordinary internal failure; in Core
// The boundary must be converted to unified ToolCancelled to stop reading and correctly close the tool life cycle at the same time.
  if (error instanceof PdfDocumentPortError && error.code === "cancelled") {
    throw createCoreError(CoreErrorType.ToolCancelled, error.message, {
      cause: error,
      recoverable: true,
    });
  }
  if (signal.aborted) {
    throw error;
  }
}

export function formatReadPdfOutput(output: ReadPdfOutput): ModelMessageContent {
  return [
    {
      type: "text",
      text: `PDF file read: ${output.filePath} (${formatFileSize(output.originalSize)})`,
    },
    {
      type: "file",
      mediaType: "application/pdf",
      name: basename(output.filePath),
      dataUrl: `data:application/pdf;base64,${output.base64}`,
      source: {
        id: "read-pdf",
        kind: "inline",
        mimeType: "application/pdf",
        placeholder: basename(output.filePath),
        sizeBytes: output.originalSize,
      },
    },
  ];
}

export function formatReadPdfPagesOutput(output: ReadPartsOutput): ModelMessageContent {
  return [
    {
      type: "text",
      text: `PDF pages extracted: ${output.numParts} page(s) from ${output.filePath} (${formatFileSize(output.originalSize)})`,
    },
    ...output.pages.map((page) => ({
      type: "image" as const,
      mediaType: page.mimeType,
      dataUrl: `data:${page.mimeType};base64,${page.base64}`,
      source: {
        id: `read-pdf-page-${page.pageNumber}`,
        kind: "inline" as const,
        mimeType: page.mimeType,
        placeholder: `PDF page ${page.pageNumber}`,
        sizeBytes: page.transformedSize ?? page.originalSize,
      },
    })),
  ];
}

export function formatFileSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} bytes`;
  if (bytes < 1024 * 1024) return `${oneDecimal(bytes / 1024)}KB`;
  if (bytes < 1024 * 1024 * 1024) return `${oneDecimal(bytes / (1024 * 1024))}MB`;
  return `${oneDecimal(bytes / (1024 * 1024 * 1024))}GB`;
}

function oneDecimal(value: number): string {
  return value.toFixed(1).replace(/\.0$/u, "");
}

function hasPdfMagic(data: Uint8Array): boolean {
  return data.byteLength >= 5 && Buffer.from(data.subarray(0, 5)).toString("ascii") === "%PDF-";
}

function isReadImageMime(value: string): value is ReadImageOutput["mimeType"] {
  return (
    value === "image/jpeg" ||
    value === "image/png" ||
    value === "image/gif" ||
    value === "image/webp"
  );
}

function failure(errorCode: ReadErrorCode, message: string): ToolHandlerFailure {
  return { result: false, errorCode, message };
}

function createToolTrace(context: ToolExecutionContext): TraceContext {
  return {
    traceId: context.traceId,
    spanId: context.spanId,
    parentSpanId: context.parentSpanId,
    sessionId: context.sessionId,
    turnId: context.turnId,
  } as TraceContext;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
