import type { FilePartSource, TurnAttachment } from "../deps.js";
import type { ResolvedTurnAttachment } from "../types.js";
import { safeAttachmentOriginalRef } from "./attachment-artifacts.js";

export function resolvedPlaceholderAttachment(
  attachment: TurnAttachment,
  placeholder: string,
  errorCode: string,
  options: {
    filename?: string;
    mime?: string;
    sizeBytes?: number;
    source?: FilePartSource;
  } = {},
): ResolvedTurnAttachment {
  const mime =
    options.mime ??
    (attachment.type === "image"
      ? "image/*"
      : attachment.type === "pdf"
        ? "application/pdf"
        : "text/plain");
  const safeOriginalRef = safeAttachmentOriginalRef(attachment);
  return {
    contentBlock: { type: "text", text: `[Attached ${mime}: ${placeholder}]` },
    filename: options.filename,
    metadata: {
      errorCode,
      originalUrl: safeOriginalRef,
      recoverability: "metadata_only",
      sizeBytes: options.sizeBytes,
      storageKind: attachment.path ? "local_ref" : "metadata_only",
    },
    mime,
    source: options.source,
// Invalid media data URL. Although it has been downgraded to placeholder text, it still passed the file part's url in the past.
//Put base64 text into session. PDF must not write data URL to part.data; recoverable artifact
// The URI is still retained to facilitate subsequent diagnosis and retry according to the established authorization path.
    url:
      attachment.type === "video" || attachment.type === "pdf"
        ? attachment.content?.startsWith("zcode-artifact://")
          ? attachment.content
          : (attachment.path ??
            (attachment.type === "pdf" ? "inline:pdf" : (safeOriginalRef ?? "")))
        : (attachment.path ?? attachment.content ?? ""),
  };
}
