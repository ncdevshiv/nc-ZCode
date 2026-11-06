/* eslint-disable max-lines -- Cohesive contract/runtime module kept in one file; split is tracked separately. */
// Storage adapters - EventStore, ArtifactStore, MemoryStore implementations

import { createHash, randomUUID } from "node:crypto";
import { mkdir, readdir, readFile, rename, stat, unlink, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";

import type {
  ImageAttachmentPathPrimeRequest,
  MediaAttachmentPathPrimeRequest,
  MediaAttachmentPathEnsureRequest,
  MediaAttachmentPathResult,
  ToolBinaryArtifactWriteRequest,
  ToolArtifactStorePort,
  ToolArtifactReadRequest,
  ToolArtifactReadResult,
  ToolArtifactStatRequest,
  ToolArtifactStatResult,
  ToolArtifactWriteRequest,
  ToolArtifactWriteResult,
  ToolBinaryArtifactReadResult,
} from "@zcode/contracts";
import { maybeThrowStorageFsFault } from "./fs-fault-injection.js";

export * from "./session-store.js";

//The memory event store implementation has been dropped to @zcode/contracts,
// Keep the export path of `@zcode/adapters/storage` unchanged here to prevent the caller from changing the import.
export {
  InMemorySessionEventStore,
  createInMemorySessionEventStore,
  type InMemorySessionEventStoreOptions,
} from "@zcode/contracts";

export interface NodeToolArtifactStoreOptions {
  imageCacheRootDir: string;
  pdfCacheRootDir?: string;
  rootDir: string;
  videoCacheRootDir: string;
}

export class NodeToolArtifactStore implements ToolArtifactStorePort {
  private readonly imageCacheRootDir: string;
  private readonly pdfCacheRootDir: string;
  private readonly rootDir: string;
  private readonly videoCacheRootDir: string;
  private readonly mediaAttachmentPathFlights = new Map<
    string,
    Promise<MediaAttachmentPathResult>
  >();

  constructor(options: NodeToolArtifactStoreOptions) {
    this.imageCacheRootDir = options.imageCacheRootDir;
    this.rootDir = options.rootDir;
    this.pdfCacheRootDir = options.pdfCacheRootDir ?? join(dirname(options.rootDir), "pdf-cache");
    this.videoCacheRootDir = options.videoCacheRootDir;
  }

  async writeToolResultArtifact(
    request: ToolArtifactWriteRequest,
    options?: { signal?: AbortSignal },
  ): Promise<ToolArtifactWriteResult> {
    if (options?.signal?.aborted) {
      throw options.signal.reason instanceof Error
        ? options.signal.reason
        : new Error("Tool artifact write cancelled");
    }

    const artifactId = `tool-result-${crypto.randomUUID()}`;
    const contentType = request.contentType ?? "application/json";
    const extension = extensionForContentType(contentType);
    const sessionDir = join(this.rootDir, sanitizePathSegment(request.sessionId));
    const fileName = `${sanitizePathSegment(String(request.toolCallId))}-${artifactId}${extension}`;
    const path = join(sessionDir, fileName);

    maybeThrowStorageFsFault({ operation: "mkdir", path: sessionDir });
    await mkdir(sessionDir, { recursive: true });
    maybeThrowStorageFsFault({ operation: "writeFile", path });
    await writeFile(path, request.content, "utf8");

    return {
      id: artifactId,
      uri: `zcode-artifact://${encodeURIComponent(request.sessionId)}/${encodeURIComponent(artifactId)}`,
      path,
      bytes: Buffer.byteLength(request.content, "utf8"),
      contentType,
      createdAt: new Date(),
    };
  }

  async writeToolResultBinaryArtifact(
    request: ToolBinaryArtifactWriteRequest,
    options?: { signal?: AbortSignal },
  ): Promise<ToolArtifactWriteResult> {
    if (options?.signal?.aborted) {
      throw options.signal.reason instanceof Error
        ? options.signal.reason
        : new Error("Tool binary artifact write cancelled");
    }

    const artifactId = `tool-result-${crypto.randomUUID()}`;
    const extension = normalizeArtifactExtension(
      request.extension ?? extensionForBinaryContentType(request.contentType),
    );
    const sessionDir = join(this.rootDir, sanitizePathSegment(request.sessionId));
    const fileName = `${sanitizePathSegment(String(request.toolCallId))}-${artifactId}${extension}`;
    const path = join(sessionDir, fileName);
    const content = Buffer.from(request.content);

    maybeThrowStorageFsFault({ operation: "mkdir", path: sessionDir });
    await mkdir(sessionDir, { recursive: true });
    maybeThrowStorageFsFault({ operation: "writeFile", path });
    await writeFile(path, content);

    return {
      id: artifactId,
      uri: `zcode-artifact://${encodeURIComponent(request.sessionId)}/${encodeURIComponent(artifactId)}`,
      path,
      bytes: content.byteLength,
      contentType: request.contentType,
      createdAt: new Date(),
    };
  }

  async readToolResultArtifact(
    request: ToolArtifactReadRequest,
    options?: { signal?: AbortSignal },
  ): Promise<ToolArtifactReadResult> {
    if (options?.signal?.aborted) {
      throw options.signal.reason instanceof Error
        ? options.signal.reason
        : new Error("Tool artifact read cancelled");
    }

    const { path, contentType, bytes } = await this.readArtifactFile(request.uri);
    const content = isTextArtifactContentType(contentType)
      ? bytes.toString("utf8")
      : bytes.toString("base64");
    return {
      uri: request.uri,
      path,
      content,
      bytes: bytes.byteLength,
      contentType,
    };
  }

  /**
* Directly read raw bytes for use by v4 chunked queries and viewers to avoid encoding and decoding changing content.
* contentType is still inferred from the file name, just for reference.
   */
  async readToolResultBinaryArtifact(
    request: ToolArtifactReadRequest,
    options?: { signal?: AbortSignal },
  ): Promise<ToolBinaryArtifactReadResult> {
    if (options?.signal?.aborted) {
      throw options.signal.reason instanceof Error
        ? options.signal.reason
        : new Error("Tool artifact read cancelled");
    }
    const { path, contentType, bytes } = await this.readArtifactFile(request.uri);
    return { uri: request.uri, path, bytes: new Uint8Array(bytes), contentType };
  }

/** The two readbacks share the same location + read file: uri → the file containing artifactId in the session directory. */
  private async readArtifactFile(
    uri: string,
  ): Promise<{ path: string; contentType: string; bytes: Buffer }> {
    const { artifactId, sessionId } = parseArtifactUri(uri);
    const sessionDir = join(this.rootDir, sanitizePathSegment(sessionId));
    const entries = await readdir(sessionDir);
    const fileName = entries.find((entry) => entry.includes(artifactId));
    if (!fileName) {
      throw new Error(`Tool artifact not found: ${uri}`);
    }
    const path = join(sessionDir, fileName);
    return { path, contentType: contentTypeForFileName(fileName), bytes: await readFile(path) };
  }

  async statToolResultArtifact(
    request: ToolArtifactStatRequest,
    options?: { signal?: AbortSignal },
  ): Promise<ToolArtifactStatResult> {
    if (options?.signal?.aborted) {
      throw options.signal.reason instanceof Error
        ? options.signal.reason
        : new Error("Tool artifact stat cancelled");
    }
    const { artifactId, sessionId } = parseArtifactUri(request.uri);
    const sessionDir = join(this.rootDir, sanitizePathSegment(sessionId));
    const entries = await readdir(sessionDir);
    const fileName = entries.find((entry) => entry.includes(artifactId));
    if (!fileName) throw new Error(`Tool artifact not found: ${request.uri}`);
    const path = join(sessionDir, fileName);
    const artifactStat = await stat(path);
    return {
      uri: request.uri,
      bytes: artifactStat.size,
      contentType: contentTypeForFileName(fileName),
      path,
      mtimeMs: artifactStat.mtimeMs,
    };
  }

  primeImageAttachmentPath(
    request: ImageAttachmentPathPrimeRequest,
  ): Promise<MediaAttachmentPathResult> {
    return this.primeMediaAttachmentPath(request);
  }

  primeMediaAttachmentPath(
    request: MediaAttachmentPathPrimeRequest,
  ): Promise<MediaAttachmentPathResult> {
    return this.runMediaAttachmentPathFlight(request.uri, () =>
      this.writeDerivedMediaAttachment(request.uri, request.mediaType, Buffer.from(request.bytes)),
    );
  }

  ensureMediaAttachmentPath(
    request: MediaAttachmentPathEnsureRequest,
  ): Promise<MediaAttachmentPathResult> {
    return this.runMediaAttachmentPathFlight(request.uri, async () => {
      const requestedPath = derivedMediaAttachmentPath(
        this.imageCacheRootDir,
        this.pdfCacheRootDir,
        this.videoCacheRootDir,
        request.uri,
        request.mediaType,
      );
      if (!requestedPath) return { status: "unsupported" };
      if (await isRegularFile(requestedPath)) return { status: "ready", path: requestedPath };

      const artifact = await this.readToolResultArtifact({ uri: request.uri });
      const decoded = decodeMediaDataUrlArtifact(
        artifact.content,
        request.uri,
        mediaKindForContentType(request.mediaType)!,
      );
      if (mediaKindForContentType(decoded.mediaType) === "pdf" && !isPdfBytes(decoded.bytes)) {
        throw new Error(`Media attachment artifact is not a PDF: ${request.uri}`);
      }
      return this.writeDerivedMediaAttachment(request.uri, decoded.mediaType, decoded.bytes);
    });
  }

  private runMediaAttachmentPathFlight(
    uri: string,
    materialize: () => Promise<MediaAttachmentPathResult>,
  ): Promise<MediaAttachmentPathResult> {
    const inFlight = this.mediaAttachmentPathFlights.get(uri);
    if (inFlight) return inFlight;

// The paste placement and the subsequent sending will enter the materialization concurrently; URI-level singleflight
// Ensure that the same write task is sent and waited, and the same derived media will not be placed repeatedly.
    let flight!: Promise<MediaAttachmentPathResult>;
    flight = materialize().finally(() => {
      if (this.mediaAttachmentPathFlights.get(uri) === flight) {
        this.mediaAttachmentPathFlights.delete(uri);
      }
    });
    this.mediaAttachmentPathFlights.set(uri, flight);
    return flight;
  }

  private async writeDerivedMediaAttachment(
    uri: string,
    mediaType: string,
    bytes: Buffer,
  ): Promise<MediaAttachmentPathResult> {
    const path = derivedMediaAttachmentPath(
      this.imageCacheRootDir,
      this.pdfCacheRootDir,
      this.videoCacheRootDir,
      uri,
      mediaType,
    );
// Existing media processing chains can accept formats that the derived cache cannot name.
// The derived path is enhanced information and should be skipped if the MIME is not supported. The original media/base64 request cannot be blocked.
    if (!path) return { status: "unsupported" };
    if (await isRegularFile(path)) return { status: "ready", path };

    const sessionDir = dirname(path);
    const temporaryPath = `${path}.tmp-${randomUUID()}`;
    maybeThrowStorageFsFault({ operation: "mkdir", path: sessionDir });
    await mkdir(sessionDir, { recursive: true });
    try {
      maybeThrowStorageFsFault({ operation: "writeFile", path: temporaryPath });
      await writeFile(temporaryPath, bytes);
      maybeThrowStorageFsFault({ operation: "rename", path });
      await rename(temporaryPath, path);
    } catch (error) {
      await unlink(temporaryPath).catch(() => undefined);
      throw error;
    }
    return { status: "ready", path };
  }
}

export function createNodeToolArtifactStore(
  options: NodeToolArtifactStoreOptions,
): ToolArtifactStorePort {
  return new NodeToolArtifactStore(options);
}

function sanitizePathSegment(value: string): string {
  return value.replace(/[^a-zA-Z0-9._-]/g, "_").slice(0, 120) || "unknown";
}

function extensionForContentType(contentType: string): string {
  const mime = contentType.split(";")[0]?.trim().toLowerCase() ?? "";
  switch (mime) {
    case "text/plain":
      return ".txt";
    case "text/markdown":
      return ".md";
    case "application/json":
      return ".json";
    case "image/png":
      return ".png";
    case "image/jpeg":
    case "image/jpg":
      return ".jpg";
    case "image/gif":
      return ".gif";
    case "image/webp":
      return ".webp";
    case "application/pdf":
      return ".pdf";
    default:
      return ".json";
  }
}

function extensionForBinaryContentType(contentType: string): string {
  const extension = extensionForContentType(contentType);
  return extension === ".json" && !contentType.toLowerCase().includes("json") ? ".bin" : extension;
}

function normalizeArtifactExtension(extension: string): string {
  const withDot = extension.startsWith(".") ? extension : `.${extension}`;
  const sanitized = withDot
    .replace(/[^a-zA-Z0-9.]/g, "")
    .slice(0, 16)
    .toLowerCase();
  return /^\.[a-z0-9]+$/.test(sanitized) ? sanitized : ".bin";
}

function contentTypeForFileName(fileName: string): string {
  const lower = fileName.toLowerCase();
  if (lower.endsWith(".txt")) return "text/plain";
  if (lower.endsWith(".md")) return "text/markdown";
  if (lower.endsWith(".png")) return "image/png";
  if (lower.endsWith(".jpg") || lower.endsWith(".jpeg")) return "image/jpeg";
  if (lower.endsWith(".gif")) return "image/gif";
  if (lower.endsWith(".webp")) return "image/webp";
  if (lower.endsWith(".pdf")) return "application/pdf";
  if (lower.endsWith(".bin")) return "application/octet-stream";
// The dwf product is placed according to the original extension (`extension` input parameter), and the inference table is filled with them. Text classes give text/*
// (utf8 is read back correctly), unknown binaries other than office files and svg are all octet-stream - **never** let another one
// The unrecognized extension falls to application/json and goes to utf8 (that is the root cause of readback corruption).
  if (lower.endsWith(".html") || lower.endsWith(".htm")) return "text/html";
  if (lower.endsWith(".csv")) return "text/csv";
  if (lower.endsWith(".svg")) return "image/svg+xml";
//The default extension for text writing is .json (default of extensionForContentType), so all existing files
// Fall in the branch above; here are only binary products (.xlsx / .docx / .pptx...) with the original extension.
  if (lower.endsWith(".json")) return "application/json";
  return "application/octet-stream";
}

function isTextArtifactContentType(contentType: string): boolean {
  return contentType === "application/json" || contentType.startsWith("text/");
}

function isPdfBytes(bytes: Uint8Array): boolean {
  return (
    bytes.byteLength >= 5 &&
    bytes[0] === 0x25 &&
    bytes[1] === 0x50 &&
    bytes[2] === 0x44 &&
    bytes[3] === 0x46 &&
    bytes[4] === 0x2d
  );
}

type DerivedMediaKind = "image" | "video" | "pdf";

function derivedMediaAttachmentPath(
  imageCacheRootDir: string,
  pdfCacheRootDir: string,
  videoCacheRootDir: string,
  uri: string,
  mediaType: string,
): string | undefined {
  const { sessionId } = parseArtifactUri(uri);
  const kind = mediaKindForContentType(mediaType);
  const extension = extensionForDerivedMediaContentType(mediaType);
  if (!kind || !extension) return undefined;
  const cacheRootDir =
    kind === "image" ? imageCacheRootDir : kind === "video" ? videoCacheRootDir : pdfCacheRootDir;
  const uriHash = createHash("sha256").update(uri).digest("hex").slice(0, 32);
  return join(cacheRootDir, sanitizePathSegment(sessionId), `${kind}-${uriHash}${extension}`);
}

function mediaKindForContentType(mediaType: string): DerivedMediaKind | undefined {
  const normalized = mediaType.split(";")[0]?.trim().toLowerCase() ?? "";
  if (normalized.startsWith("image/")) return "image";
  if (normalized.startsWith("video/")) return "video";
  if (normalized === "application/pdf") return "pdf";
  return undefined;
}

function extensionForDerivedMediaContentType(mediaType: string): string | undefined {
  const normalized = mediaType.split(";")[0]?.trim().toLowerCase();
  switch (normalized) {
    case "image/png":
      return ".png";
    case "image/jpeg":
    case "image/jpg":
      return ".jpg";
    case "image/gif":
      return ".gif";
    case "image/webp":
      return ".webp";
    case "video/mp4":
      return ".mp4";
    case "video/quicktime":
      return ".mov";
    case "video/webm":
      return ".webm";
    case "video/x-matroska":
      return ".mkv";
    case "video/x-m4v":
      return ".m4v";
    case "video/x-msvideo":
      return ".avi";
    case "application/pdf":
      return ".pdf";
    default:
      return undefined;
  }
}

function decodeMediaDataUrlArtifact(
  content: string,
  uri: string,
  expectedKind: DerivedMediaKind,
): { bytes: Buffer; mediaType: string } {
  const commaIndex = content.indexOf(",");
  const headerParts =
    content.slice(0, "data:".length).toLowerCase() === "data:" && commaIndex >= 0
      ? content.slice("data:".length, commaIndex).split(";")
      : [];
  const mediaType = headerParts.shift()?.trim();
  if (
    mediaKindForContentType(mediaType ?? "") !== expectedKind ||
    headerParts.at(-1)?.trim().toLowerCase() !== "base64" ||
    commaIndex < 0
  ) {
    throw new Error(`Media attachment artifact is not a base64 ${expectedKind} data URL: ${uri}`);
  }
  const bytes = Buffer.from(content.slice(commaIndex + 1), "base64");
  if (bytes.byteLength === 0) {
    throw new Error(`Media attachment artifact is empty: ${uri}`);
  }
  return { bytes, mediaType: mediaType! };
}

async function isRegularFile(path: string): Promise<boolean> {
  try {
    return (await stat(path)).isFile();
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") return false;
    throw error;
  }
}

function parseArtifactUri(uri: string): { artifactId: string; sessionId: string } {
  let parsed: URL;
  try {
    parsed = new URL(uri);
  } catch (error) {
    throw new Error(`Invalid tool artifact URI: ${uri}`, {
      cause: error instanceof Error ? error : undefined,
    });
  }

  if (parsed.protocol !== "zcode-artifact:") {
    throw new Error(`Unsupported tool artifact URI: ${uri}`);
  }

  const sessionId = decodeURIComponent(parsed.hostname);
  const artifactId = decodeURIComponent(parsed.pathname.replace(/^\/+/, ""));
  if (!sessionId || !artifactId) {
    throw new Error(`Invalid tool artifact URI: ${uri}`);
  }

  return { artifactId, sessionId };
}
export * from "./workspace-hook-trust-store.js";
