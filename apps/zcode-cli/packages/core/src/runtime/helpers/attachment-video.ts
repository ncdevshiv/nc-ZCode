// Mime inference and size checking shared by video attachments.
// Read tool (read-video.ts) and prompt attachment parsing (attachments.ts) are taken from here.
// Ensure that the extension → mime mapping has only one fact; the only source of truth for the mime enumeration is contracts (ReadVideoOutput).
import type { ReadVideoOutput } from "@zcode/contracts";
import { base64PayloadByteLength, isStrictBase64Payload } from "./attachment-data-url.js";

export type VideoInputMimeType = ReadVideoOutput["mimeType"];

const VIDEO_INPUT_MIME_BY_EXTENSION: Record<string, VideoInputMimeType> = {
  ".mp4": "video/mp4",
  ".m4v": "video/x-m4v",
  ".mov": "video/quicktime",
  ".webm": "video/webm",
  ".mkv": "video/x-matroska",
  ".avi": "video/x-msvideo",
};

/** Infer video mime by extension; returns undefined for unsupported video extensions. */
export function inferVideoMimeFromPath(path: string): VideoInputMimeType | undefined {
  const lower = path.toLowerCase();
  for (const [extension, mime] of Object.entries(VIDEO_INPUT_MIME_BY_EXTENSION)) {
    if (lower.endsWith(extension)) return mime;
  }
  return undefined;
}

export function parseInlineVideoDataUrl(
  dataUrl: string,
): { mediaType: string; sizeBytes: number } | undefined {
  const match = /^data:([^;,]+);base64,(.*)$/i.exec(dataUrl);
  const mediaType = match?.[1]?.toLowerCase();
  const payload = match?.[2];
// video inline used to only check the loose data URL header, missing the base64 tag,
// Illegal text and non-video MIME will continue to the persistent or general text branch.
  if (
    !mediaType?.startsWith("video/") ||
    payload === undefined ||
    !isStrictBase64Payload(payload)
  ) {
    return undefined;
  }
  return { mediaType, sizeBytes: base64PayloadByteLength(payload) };
}
