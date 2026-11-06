// Strict canonical parsing of Plugin dialogue references.
//Contract:
// - Only accept Markdown link destinations in the form of `plugin://stable-id`, and the protocol name is case-sensitive (lowercase only).
// - stable-id must be `name@marketplace`, both segments match [A-Za-z0-9][A-Za-z0-9._-]*, total length ≤ 256.
// - Reject query, fragment, credentials, whitespace, control characters, and `%` (no implicit percent-decoding).
// - Identity only comes from destination; label never participates in parsing.

const PLUGIN_REFERENCE_SCHEME = "plugin://";
// Consistent with the link syntax of mentionMarkdown: label supports \ escaping, and destination supports <...> or bare form.
const MARKDOWN_LINK_PATTERN =
  /\[(?:\\.|[^\\\]])*\]\((?:<((?:\\.|[^>])*?)>|((?:\\.|[^)\s])*))\)/g;
const PLUGIN_ID_SEGMENT_PATTERN = /^[A-Za-z0-9][A-Za-z0-9._-]*$/;

export const MAX_PLUGIN_REFERENCES_PER_TURN = 8;
const MAX_PLUGIN_STABLE_ID_LENGTH = 256;

/**
* Verify whether a candidate string is a strictly legal Plugin stable ID (`name@marketplace`).
* This check is used for defense on both parser and reminder output sides (fail closed).
 */
export function isValidPluginStableId(candidate: string): boolean {
  if (candidate.length === 0 || candidate.length > MAX_PLUGIN_STABLE_ID_LENGTH) {
    return false;
  }
  const separatorIndex = candidate.indexOf("@");
  if (separatorIndex <= 0 || separatorIndex !== candidate.lastIndexOf("@")) {
    return false;
  }
  const name = candidate.slice(0, separatorIndex);
  const marketplace = candidate.slice(separatorIndex + 1);
// Segment-level character set verification is the complete security boundary: query/fragment/credentials/blank/control characters/% are not present
// Within the [A-Za-z0-9._-] set, all are rejected without any implicit percent-decoding or tolerant matching.
  return PLUGIN_ID_SEGMENT_PATTERN.test(name) && PLUGIN_ID_SEGMENT_PATTERN.test(marketplace);
}

function parsePluginDestination(destination: string): string | null {
//The protocol name is case-sensitive: neither `Plugin://` nor `PLUGIN://` is accepted.
  if (!destination.startsWith(PLUGIN_REFERENCE_SCHEME)) {
    return null;
  }
  const stableId = destination.slice(PLUGIN_REFERENCE_SCHEME.length);
  if (!isValidPluginStableId(stableId)) {
    return null;
  }
  return stableId;
}

function isPluginSchemeDestination(destination: string): boolean {
// Only destinations that are "intentionally plugin protocol" are counted in the invalid statistics;
// Uppercase and lowercase variants (Plugin://, etc.) are also counted as intent hits but fail to parse, preventing label spoofing and bypassing statistics.
  return /^plugin:\/\//i.test(destination);
}

export interface ExtractPluginReferencesResult {
/** References in the order of first appearance in the text and after deduplication by stable ID. */
  references: string[];
/** The number of references that are discarded beyond the upper limit of a single round (fail closed, call side note debug truncated). */
  truncatedCount: number;
/** The number of destinations that hit the plugin protocol intent but failed to parse (format-level rejection before unknown). */
  invalidCount: number;
}

/**
* Extract Plugin reference (stable ID) from canonical user text.
* The identity only comes from the link destination; the Markdown label does not participate at all.
 */
export function extractPluginReferences(input: string): ExtractPluginReferencesResult {
  const references: string[] = [];
  const seen = new Set<string>();
  let truncatedCount = 0;
  let invalidCount = 0;

  MARKDOWN_LINK_PATTERN.lastIndex = 0;
  for (const match of input.matchAll(MARKDOWN_LINK_PATTERN)) {
    const destination = match[1] ?? match[2] ?? "";
    if (!isPluginSchemeDestination(destination)) {
      continue;
    }
    const stableId = parsePluginDestination(destination);
    if (stableId === null) {
      invalidCount++;
      continue;
    }
    if (seen.has(stableId)) {
      continue;
    }
    if (references.length >= MAX_PLUGIN_REFERENCES_PER_TURN) {
      truncatedCount++;
      continue;
    }
    seen.add(stableId);
    references.push(stableId);
  }

  return { references, truncatedCount, invalidCount };
}
