import { isMap, parseDocument } from "yaml";

import { memoryFileRelativePath } from "./memory-file-path.js";

const MEMORY_FRONTMATTER_PATTERN =
  /^(\uFEFF?---(?:\r\n|\n))([\s\S]*?)((?:\r\n|\n)---)(?=(?:\r\n|\n)|$)/u;

export function stampMemoryOriginSessionId(input: {
  content: string;
  filePath: string;
  memoryRoot: string | undefined;
  sessionId: string;
}): string {
  if (
    !input.memoryRoot ||
    !input.filePath.endsWith(".md") ||
    memoryFileRelativePath(input.memoryRoot, input.filePath) === undefined
  ) {
    return input.content;
  }

  const match = MEMORY_FRONTMATTER_PATTERN.exec(input.content);
  if (!match) return input.content;

  const opening = match[1]!;
  const frontmatter = match[2]!;
  const closing = match[3]!;
  const document = parseDocument(frontmatter);
  if (document.errors.length > 0) return input.content;

  try {
    if (document.get("metadata", true) === undefined) document.set("metadata", {});
    const metadata = document.get("metadata", true);
    if (!isMap(metadata)) return input.content;

    const existingOrigin = metadata.get("originSessionId");
    if (typeof existingOrigin === "string" && existingOrigin.length > 0) return input.content;

// The baseline only serializes frontmatter when origin is missing, and completes node_type in the same write.
    metadata.delete("node_type");
    metadata.items.unshift(document.createPair("node_type", "memory"));
    metadata.set("originSessionId", input.sessionId);
  } catch {
// Non-mapping metadata will not be repaired; the Memory file format is still constrained by prompt.
    return input.content;
  }

  const lineEnding = opening.endsWith("\r\n") ? "\r\n" : "\n";
  const stampedFrontmatter = document.toString().replace(/\n$/u, "").replace(/\n/gu, lineEnding);
  return `${opening}${stampedFrontmatter}${closing}${input.content.slice(match[0].length)}`;
}
