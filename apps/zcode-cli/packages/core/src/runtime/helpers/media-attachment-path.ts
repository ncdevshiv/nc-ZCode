import type {
  ModelInputMessage,
  ModelMessageContentBlock,
  ToolArtifactStorePort,
} from "../deps.js";

type PathProjectableMediaBlock =
  | Extract<ModelMessageContentBlock, { type: "image" | "video" }>
  | (Extract<ModelMessageContentBlock, { type: "file" }> & { mediaType: "application/pdf" });

export async function projectMessagesWithMediaAttachmentPaths(
  messages: ModelInputMessage[],
  artifactStore: ToolArtifactStorePort | undefined,
): Promise<ModelInputMessage[]> {
  let changed = false;
  const projected = await Promise.all(
    messages.map(async (message) => {
      if (message.role !== "user" || !Array.isArray(message.content)) return message;

      const mediaBlocks = message.content.filter(isPathProjectableUserMedia);
      if (mediaBlocks.length === 0) return message;
      const paths = await Promise.all(
        mediaBlocks.map((block) => resolveMediaAttachmentPath(block, artifactStore)),
      );
      const pathByBlock = new Map<PathProjectableMediaBlock, string>();
      mediaBlocks.forEach((block, index) => {
        const path = paths[index];
        if (path) pathByBlock.set(block, path);
      });
      if (pathByBlock.size === 0) return message;

      changed = true;
      const content = message.content.map((block) => {
        if (!isPathProjectableUserMedia(block) || !pathByBlock.has(block)) {
          return cloneContentBlock(block);
        }
        return {
          ...block,
          source: { ...block.source!, path: pathByBlock.get(block)! },
        };
      });
      content.push(
        ...[...pathByBlock.entries()].map(([block, path]) => ({
          type: "text" as const,
          text: mediaSourceText(block.type, path),
        })),
      );
      return {
        ...message,
        cacheControl: message.cacheControl ? { ...message.cacheControl } : undefined,
        content,
        toolCalls: message.toolCalls?.map((toolCall) => ({ ...toolCall })),
      };
    }),
  );
  return changed ? projected : messages;
}

function isPathProjectableUserMedia(
  block: ModelMessageContentBlock,
): block is PathProjectableMediaBlock {
// inline only represents provider-ready media and does not guarantee the existence of rebuildable durable artifacts.
// path is an optional enhancement; only artifact URI or existing local path will participate in materialization to avoid blocking existing media sending.
  return (
    (block.type === "image" || block.type === "video" || isPdfBlock(block)) &&
    ((block.source?.kind === "inline" &&
      block.source.uri?.startsWith("zcode-artifact://") === true) ||
      (block.source?.kind === "local_file" && Boolean(block.source.path)))
  );
}

async function resolveMediaAttachmentPath(
  block: PathProjectableMediaBlock,
  artifactStore: ToolArtifactStorePort | undefined,
): Promise<string | undefined> {
  const source = block.source!;
  const mediaType = isPdfBlock(block) ? "pdf" : block.type;
  if (source.kind === "local_file" && source.path) return source.path;

  const uri = source.uri;
  if (!uri?.startsWith("zcode-artifact://") || !artifactStore?.ensureMediaAttachmentPath) {
    throw mediaAttachmentMaterializationError(mediaType, source.placeholder ?? uri ?? source.id);
  }
  try {
// The derived path of inline is not a session fact, even if the old path is accidentally left in the input,
// Must also be rechecked with the durable artifact URI, which will be rebuilt by the store if missing.
    const result = await artifactStore.ensureMediaAttachmentPath({
      mediaType: source.mimeType ?? block.mediaType,
      uri,
    });
    if (result.status === "unsupported") return undefined;
    if (!result.path.trim()) throw new Error("empty derived path");
    return result.path;
  } catch (error) {
    throw mediaAttachmentMaterializationError(mediaType, source.placeholder ?? uri, error);
  }
}

function mediaAttachmentMaterializationError(
  type: "image" | "video" | "pdf",
  label: string,
  cause?: unknown,
): Error {
  return new Error(`Unable to materialize ${type} attachment path: ${label}`, {
    cause: cause instanceof Error ? cause : undefined,
  });
}

function mediaSourceText(type: PathProjectableMediaBlock["type"], path: string): string {
  const label = type === "image" ? "Image" : type === "video" ? "Video" : "PDF";
  return `[${label}: source: ${path}]`;
}

function isPdfBlock(block: ModelMessageContentBlock): block is Extract<
  ModelMessageContentBlock,
  { type: "file" }
> & {
  mediaType: "application/pdf";
} {
  return (
    block.type === "file" &&
    block.mediaType.split(";", 1)[0]?.trim().toLowerCase() === "application/pdf"
  );
}

function cloneContentBlock(block: ModelMessageContentBlock): ModelMessageContentBlock {
  if (block.type === "image" || block.type === "video" || block.type === "file") {
    return { ...block, source: block.source ? { ...block.source } : undefined };
  }
  return { ...block };
}
