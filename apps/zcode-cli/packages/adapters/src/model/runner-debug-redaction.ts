import { sanitizeModelNetworkHeaders } from "./runner-network-headers.js";

export function sanitizeModelIODebugRecord(
  record: Record<string, unknown>,
): Record<string, unknown> {
  const request = sanitizeModelIOSectionHeaders(asRecord(record.request));
  const response = sanitizeModelIOSectionHeaders(asRecord(record.response));
  return redactModelIOImageAndVideoData({
    ...record,
    ...(request ? { request } : {}),
    ...(response ? { response } : {}),
  }) as Record<string, unknown>;
}

function sanitizeModelIOSectionHeaders(
  section: Record<string, unknown> | undefined,
): Record<string, unknown> | undefined {
  if (!section || !Object.hasOwn(section, "headers")) {
    return section;
  }
  return {
    ...section,
    headers: sanitizeModelNetworkHeaders(section.headers),
  };
}

function asRecord(value: unknown): Record<string, unknown> | undefined {
  return typeof value === "object" && value !== null && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : undefined;
}

//The media key names of internal dataUrl, AI SDK image/file and provider wire are different.
// Desensitization scattered on the caller will miss the image/video state; unified processing at the model-I/O drop boundary is recursive.
function redactModelIOImageAndVideoData(value: unknown): unknown {
  if (Array.isArray(value)) {
    return value.map((item) => redactModelIOImageAndVideoData(item));
  }
  if (!value || typeof value !== "object") {
    return value;
  }
  const record = value as Record<string, unknown>;
  const rawDataMediaType =
    record.type === "base64" && typeof record.media_type === "string"
      ? record.media_type
      : (record.type === "file" || record.type === "image") && typeof record.mediaType === "string"
        ? record.mediaType
        : undefined;
  const rawDataKey = record.type === "image" ? "image" : "data";
  const isRawImageOrVideo = isImageOrVideoMimeType(rawDataMediaType);
  const result: Record<string, unknown> = {};
  for (const [key, child] of Object.entries(record)) {
    if (typeof child === "string") {
      const dataUrlMimeType = readDataUrlMimeType(child);
      if (isImageOrVideoMimeType(dataUrlMimeType)) {
        result[key] = `[${key} omitted from model-io: ${dataUrlMimeType}, ${child.length} chars]`;
        continue;
      }
      if (key === rawDataKey && isRawImageOrVideo) {
        result[key] = `[${key} omitted from model-io: ${rawDataMediaType}, ${child.length} chars]`;
        continue;
      }
    }
    result[key] = redactModelIOImageAndVideoData(child);
  }
  return result;
}

function readDataUrlMimeType(dataUrl: string): string {
  const match = /^data:([^;,]+)[;,]/.exec(dataUrl);
  return match?.[1] ?? "unknown";
}

function isImageOrVideoMimeType(mimeType: string | undefined): boolean {
// MIME type is not case-sensitive. Direct comparison will cause non-standard cases to bypass log desensitization.
  const normalized = mimeType?.trim().toLowerCase();
  return normalized?.startsWith("image/") === true || normalized?.startsWith("video/") === true;
}
