import type { ModelMessage as AiSdkModelMessage } from "ai";
import {
  getUnsupportedModelInputMediaKind,
  modelMessageContentToText,
  type ModelInputFormat,
  type ModelMessageContent,
  type ModelMessageContentBlock,
} from "@zcode/contracts";
import { containsOfficialCuaImageRefCredentialText } from "@zcode/zcode-cua/frame-contract";
import { dataUrlToDataContent, unsupportedInputMediaText } from "./media-transform-policy.js";

type AiSdkUserContent = Extract<AiSdkModelMessage, { role: "user" }>["content"];
type AiSdkUserContentParts = Extract<AiSdkUserContent, unknown[]>;

interface ToolResultMediaProjectionOptions {
  apiFormat?: string;
  providerKind?: "openai" | "anthropic" | "openai-compatible" | "gateway" | "custom";
  stripMedia?: boolean;
  inputFormat?: ModelInputFormat;
  toolName: string;
}

const CHAT_STYLE_TOOL_RESULT_PROVIDER_KINDS = new Set(["openai-compatible", "gateway"]);
const TOOL_RESULT_MEDIA_INTRO_PREFIX = "Tool result media from";

export function shouldTextifyStructuredToolResults(
  options: Pick<ToolResultMediaProjectionOptions, "apiFormat" | "providerKind">,
): boolean {
  if (options.apiFormat !== undefined) {
    return options.apiFormat === "openai-chat-completions";
  }
// Root cause: openai kind uses Responses API; judged by the broad OpenAI-like family, it
// Mistakenly projected into tool text + synthetic user of Chat Completions. Only true when explicit format is missing
// Provider fallback that does use Chat style results, explicit apiFormat still has the highest priority.
  return (
    options.providerKind !== undefined &&
    CHAT_STYLE_TOOL_RESULT_PROVIDER_KINDS.has(options.providerKind)
  );
}

/**
* Whether the tool result contains video media. AI SDK tool result part has no video variant (other than image-data
* Only supports file-data/pdf), so tool results containing video are available in all provider kinds (including anthropopic)
* Textify + post-user part projection must be used, otherwise the video content will be lost at the wire boundary.
 */
export function toolResultHasVideoMedia(content: ModelMessageContent): boolean {
  if (typeof content === "string") return false;
  return content.some((block) => block.type === "video");
}

/**
* Pairing rule (producer predicate driven): immediately after the media block and the content is protected media
* The text block of the certificate (image_ref issued by the producer, etc.) is regarded as the paired text of the media.
* When provider projection delays media into independent user messages (openai-like textify), pairing
* The text must be deferred together and in order - leaving it in the tool text will cause the model to see "referenced text" and
* "Referenced media" belongs to two messages, and multi-frame scenes are easily mismatched.
* Ordinary text (description, summary) is not a voucher and does not participate in pairing - the media semantics of general tools are maintained
* Unchanged; the certificate determination is part of the producer frame contract, and the host is referenced by frame-contract.
* Producer canonical order is image-first (raster first, image_ref immediately after).
* Pair lookup reads its authority backwards from the image and does not rely on the provider's special truncation behavior.
 */
type PairedTextOptions = Pick<ToolResultMediaProjectionOptions, "stripMedia" | "inputFormat">;

function isPairedTextBlock(
  content: ModelMessageContentBlock[],
  index: number,
  options: PairedTextOptions,
): boolean {
  const block = content[index];
  if (block?.type !== "text" || block.text.trim().length === 0) return false;
// Only media credentials issued by the producer are paired; any adjacent text pairing will change the
// Universal media semantics (adjacent descriptions of ordinary screenshots/file previews are not media).
  if (!containsOfficialCuaImageRefCredentialText(block.text)) return false;
// Skip the blank text and search for earlier images will destroy the image -> image_ref specified by the producer
// Direct adjacencies, and possibly reauthorizing frame_refs that have been split by inserted blocks.
  const candidate = content[index - 1];
  return (
    candidate !== undefined &&
    contentBlockToUserMediaParts(candidate, {
      ...options,
      toolName: "",
    }).length > 0
  );
}

function pairedTextIndexes(
  content: ModelMessageContentBlock[],
  options: PairedTextOptions,
): Set<number> {
// media will not be delayed when stripMedia (toToolResultMediaUserParts directly returns empty),
// Pairing omissions must not precede - otherwise the text disappears from the tool output and does not appear in any messages.
  if (options.stripMedia === true) return new Set();
  const paired = new Set<number>();
  for (let index = 0; index < content.length; index += 1) {
    if (isPairedTextBlock(content, index, options)) paired.add(index);
  }
  return paired;
}

export function toStructuredToolResultText(
  content: ModelMessageContent,
  options: Pick<ToolResultMediaProjectionOptions, "inputFormat"> = {},
): string {
  if (typeof content === "string") return content;
  const paired = pairedTextIndexes(content, options);
  return modelMessageContentToText(
    content.map((block, index) => {
// Paired text is deferred with media (see toToolResultMediaUserParts), omitted here
// To prevent the model from seeing two reference texts in the same request.
      if (paired.has(index)) return { type: "text", text: "" };
      const unsupportedText = unsupportedInputMediaText(block, options.inputFormat);
      return unsupportedText ? { type: "text", text: unsupportedText } : block;
    }),
  );
}

export function toToolResultMediaUserParts(
  content: ModelMessageContent,
  options: ToolResultMediaProjectionOptions,
): AiSdkUserContentParts {
  if (typeof content === "string" || options.stripMedia) return [];
  const blocks = content;
  const paired = pairedTextIndexes(blocks, options);

  const parts: AiSdkUserContentParts = [
    {
      type: "text",
      text: `${TOOL_RESULT_MEDIA_INTRO_PREFIX} ${options.toolName}:`,
    },
  ];
// The delimiter is only used to separate "two consecutive pieces of paired text". The original criterion is parts.length > 1, which implicitly assumes pairing
// Text is always the first to enter the queue (old text-first layout); after producer is changed to image-first, raster
//Enqueued first, this criterion will insert an extra empty text block between image and its image_ref.
  let lastPushedWasPairedText = false;
  for (let index = 0; index < blocks.length; index += 1) {
    const block = blocks[index]!;
    if (block.type === "text" && paired.has(index)) {
// Paired text and adjacent media enter the deferred message together, maintaining the original order.
      if (lastPushedWasPairedText) parts.push({ type: "text", text: "\n\n" });
      parts.push({ type: "text", text: block.text });
      lastPushedWasPairedText = true;
      continue;
    }
    const mediaParts = contentBlockToUserMediaParts(block, options);
    if (mediaParts.length === 0) continue;
    parts.push(...mediaParts);
    lastPushedWasPairedText = false;
  }
  return parts.length > 1 ? parts : [];
}

function contentBlockToUserMediaParts(
  block: ModelMessageContentBlock,
  options: ToolResultMediaProjectionOptions,
): AiSdkUserContentParts {
  switch (block.type) {
    case "image": {
      if (options.inputFormat && getUnsupportedModelInputMediaKind(block, options.inputFormat)) {
        return [];
      }
      const data = dataUrlToDataContent(block.dataUrl);
      if (!data) return [];
      return [{ type: "image", image: data.data, mediaType: block.mediaType }];
    }

    case "video": {
      if (options.inputFormat && getUnsupportedModelInputMediaKind(block, options.inputFormat)) {
        return [];
      }
      const data = dataUrlToDataContent(block.dataUrl);
      if (!data) return [];
// Consistent with the user message side: video/* file part is handed over to the provider after patching to include video_url / video block.
      return [{ type: "file", data: data.data, mediaType: block.mediaType }];
    }

    case "file": {
      if (block.text !== undefined && block.text.length > 0) return [];
      if (options.inputFormat && getUnsupportedModelInputMediaKind(block, options.inputFormat)) {
        return [];
      }
      const data = block.dataUrl ? dataUrlToDataContent(block.dataUrl) : undefined;
      if (!data) return [];
      return [
        {
          type: "file",
          data: data.data,
          filename: block.name,
          mediaType: block.mediaType,
        },
      ];
    }

    case "text":
    case "reasoning":
    case "resource_link":
      return [];
  }
}

/**
* General fail-closed guard (coordinate frame references must not reach the model with "reference present, grid not present"):
* Structured tool results carry frame reference text (exact JSON such as CUA image_ref), while their media
* When it cannot be delivered because the model does not support it (or stripMedia), the result must be completely errored - placeholder replacement
* Will leave an actionable reference to induce the model to take action on the coordinates of the unseen screen. The rules only depend on
* The structural combination of "quoted text + undeliverable media" is not aware of specific tools.
 */
export function undeliverableFrameReferenceText(
  content: ModelMessageContent,
  options: Pick<ToolResultMediaProjectionOptions, "stripMedia" | "inputFormat">,
): string | undefined {
  if (!Array.isArray(content)) return undefined;
// Same inline scan detector as MCP normalization/hook - wrap image_ref in description text
// Cannot bypass fail-closed (security boundary does not depend on payload shape).
  const frameReferenceIndexes = content.flatMap((block, index) =>
    block.type === "text" && containsOfficialCuaImageRefCredentialText(block.text) ? [index] : [],
  );
  if (frameReferenceIndexes.length === 0) return undefined;
  const unavailable =
    "This tool returned a coordinate frame reference without a deliverable raster. " +
    "No image_ref or raster was exposed; do not use frame-bound coordinates. " +
    "Switch to an image-capable model and capture a new raster first.";
  if (options.stripMedia === true) return unavailable;
// Each credential must independently satisfy deliverability by its immediately preceding raster; other irrelevant images in the result
// Cannot endorse orphan ref.
  return frameReferenceIndexes.every((index) => isPairedTextBlock(content, index, options))
    ? undefined
    : unavailable;
}
