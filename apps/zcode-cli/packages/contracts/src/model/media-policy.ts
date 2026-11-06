import {
  modelMessageContentToText,
  type ModelInputFormat,
  type ModelMessageContentBlock,
} from "./index.js";

export type UnsupportedModelInputMediaKind = "image input" | "PDF input" | "video input";

export function isProviderVisibleModelInputMediaBlock(block: ModelMessageContentBlock): boolean {
  return (
    block.type === "image" ||
    isProviderVisiblePdfModelInputBlock(block) ||
    isProviderVisibleVideoModelInputBlock(block)
  );
}

export function isProviderVisiblePdfModelInputBlock(block: ModelMessageContentBlock): boolean {
  return (
    block.type === "file" &&
    block.mediaType.split(";", 1)[0]?.trim().toLowerCase() === "application/pdf" &&
    block.dataUrl !== undefined &&
// When the PDF extracted text is an empty string, the adapter will fall back to sending file-data.
// Only non-empty text can replace provider-visible PDF file data.
    (block.text === undefined || block.text.length === 0)
  );
}

export function isProviderVisibleVideoModelInputBlock(block: ModelMessageContentBlock): boolean {
  return (
    (block.type === "video" || block.type === "file") &&
    block.mediaType.toLowerCase().startsWith("video/") &&
    block.dataUrl !== undefined
  );
}

export function getUnsupportedModelInputMediaKind(
  block: ModelMessageContentBlock,
  inputFormat: ModelInputFormat,
): UnsupportedModelInputMediaKind | undefined {
  if (block.type === "image" && !inputFormat.supportsImage) {
    return "image input";
  }
  if (isProviderVisiblePdfModelInputBlock(block) && !inputFormat.supportsPdf) {
    return "PDF input";
  }
  if (isProviderVisibleVideoModelInputBlock(block) && !inputFormat.supportsVideo) {
    return "video input";
  }
  return undefined;
}

export function createUnsupportedModelInputMediaText(
  block: ModelMessageContentBlock,
  unsupportedKind: UnsupportedModelInputMediaKind,
): string {
  const placeholder = modelMessageContentToText([block]) || "[Attached media]";
  return `${placeholder}\n[Media omitted from provider request because the selected model does not support ${unsupportedKind}.]`;
}
