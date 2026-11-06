// ============================================================
// Tool Artifact Store Port - large tool result storage boundary
// ============================================================

import type { SessionId, ToolCallId, TurnId } from "./shared.js";
import type { TraceContext } from "../tracing/tracer.js";

export type ToolArtifactRetention = "session" | "project" | "temporary";

export interface ToolArtifactWriteRequest {
  sessionId: SessionId;
  turnId?: TurnId;
  toolCallId: ToolCallId | string;
  toolName: string;
  content: string;
  contentType?: string;
  retention?: ToolArtifactRetention;
  trace?: TraceContext;
}

export interface ToolBinaryArtifactWriteRequest {
  sessionId: SessionId;
  turnId?: TurnId;
  toolCallId: ToolCallId | string;
  toolName: string;
  content: Uint8Array;
  contentType: string;
  extension?: string;
  retention?: ToolArtifactRetention;
  trace?: TraceContext;
}

export interface ToolArtifactWriteResult {
  id: string;
  uri: string;
  path?: string;
  bytes: number;
  contentType: string;
  createdAt: Date;
}

export interface ToolArtifactReadRequest {
  uri: string;
  trace?: TraceContext;
}

export interface ToolArtifactReadResult {
  uri: string;
  content: string;
  contentType: string;
  bytes: number;
  path?: string;
}

/**
* Binary readback: **raw bytes**, without any text/base64 encoding.
 *
 * {@link ToolArtifactStorePort.readToolResultArtifact}
* Treat "Push contentType again according to the file name → the text will be utf8, and the rest will be base64" as the inverse operation of writing the encoding, and
* The inference table only recognizes txt/md/png/jpg/gif/webp/pdf/bin, `.xlsx` / `.docx` / `.pptx` falls to the default
* `application/json` is treated as utf8 decoded - office files are corrupted and unrecoverable when read back. consumer of bytes
* (v4 chunked query, viewer) requires the bytes themselves, and the encoding is for the model text; the two things are separated into two methods.
* `contentType` is still inferred from the file name, and is only for callers who lack a better source (the one recorded by dwf in journal).
 */
export interface ToolBinaryArtifactReadResult {
  uri: string;
  bytes: Uint8Array;
  contentType: string;
  path?: string;
}

export interface ToolArtifactStatRequest {
  uri: string;
  trace?: TraceContext;
}

export interface ToolArtifactStatResult {
  uri: string;
  bytes: number;
  contentType: string;
  path?: string;
  mtimeMs?: number;
}

export interface ImageAttachmentPathPrimeRequest {
  uri: string;
  bytes: Uint8Array;
  mediaType: string;
}

export interface MediaAttachmentPathPrimeRequest {
  uri: string;
  bytes: Uint8Array;
  mediaType: string;
}

export interface MediaAttachmentPathEnsureRequest {
  uri: string;
  mediaType: string;
}

export type MediaAttachmentPathResult =
  | { status: "ready"; path: string }
  | { status: "unsupported" };

export interface ToolArtifactStorePort {
  writeToolResultArtifact(
    request: ToolArtifactWriteRequest,
    options?: { signal?: AbortSignal },
  ): Promise<ToolArtifactWriteResult>;
  writeToolResultBinaryArtifact?(
    request: ToolBinaryArtifactWriteRequest,
    options?: { signal?: AbortSignal },
  ): Promise<ToolArtifactWriteResult>;
  readToolResultArtifact(
    request: ToolArtifactReadRequest,
    options?: { signal?: AbortSignal },
  ): Promise<ToolArtifactReadResult>;
  /**
* Read back raw bytes (see {@link ToolBinaryArtifactReadResult}). Optional: with
* {@link writeToolResultBinaryArtifact} is the same as the standard, and store implementations without binary capabilities do not need to be run with;
* Consumer side `typeof` detection, the absence means "this store cannot provide bytes" - the text must not be returned for reading and decoding.
   */
  readToolResultBinaryArtifact?(
    request: ToolArtifactReadRequest,
    options?: { signal?: AbortSignal },
  ): Promise<ToolBinaryArtifactReadResult>;
  statToolResultArtifact?(
    request: ToolArtifactStatRequest,
    options?: { signal?: AbortSignal },
  ): Promise<ToolArtifactStatResult>;
  primeImageAttachmentPath?(
    request: ImageAttachmentPathPrimeRequest,
  ): Promise<MediaAttachmentPathResult>;
  primeMediaAttachmentPath?(
    request: MediaAttachmentPathPrimeRequest,
  ): Promise<MediaAttachmentPathResult>;
  ensureMediaAttachmentPath?(
    request: MediaAttachmentPathEnsureRequest,
  ): Promise<MediaAttachmentPathResult>;
}
