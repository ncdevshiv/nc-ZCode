import type { RuntimeInputValidationIssue } from "./input-normalization.js";
import {
  createCustomIssue,
  createInvalidFormatIssue,
  createTooBigIssue,
  createTooSmallIssue,
  type ToolInputValidationIssue,
  type ToolInputValidationPath,
} from "./tool-input-validation-issues.js";
import type { ToolEntry } from "./types.js";

export function createInitialInputValidationModelContent(
  entry: ToolEntry,
  jsonIssues: readonly ToolInputValidationIssue[],
  runtimeIssues: readonly RuntimeInputValidationIssue[] | undefined,
): string {
  const issues = projectInitialModelValidationIssues(entry.inputSchema, jsonIssues, runtimeIssues);
  return `<tool_use_error>InputValidationError: ${formatToolInputValidationError(
    entry.metadata.name,
    issues,
  )}</tool_use_error>`;
}

function formatToolInputValidationError(
  toolName: string,
  issues: readonly ToolInputValidationIssue[],
): string {
  const missingParameters = issues
    .filter(
      (issue) => issue.code === "invalid_type" && issue.message.includes("received undefined"),
    )
    .map((issue) => formatValidationPath(issue.path));
  const unexpectedParameters = issues.flatMap((issue) =>
    issue.code === "unrecognized_keys" ? issue.keys : [],
  );
  const wrongTypes = issues.flatMap((issue) => {
    if (issue.code !== "invalid_type" || issue.message.includes("received undefined")) {
      return [];
    }
    return [
      {
        expected: issue.expected,
        param: formatValidationPath(issue.path),
        received: issue.message.match(/received (\w+)/)?.[1] ?? "unknown",
      },
    ];
  });

  const lines: string[] = [
    ...missingParameters.map((parameter) => `The required parameter \`${parameter}\` is missing`),
    ...unexpectedParameters.map(
      (parameter) => `An unexpected parameter \`${parameter}\` was provided`,
    ),
    ...wrongTypes.map(
      ({ expected, param, received }) =>
        `The parameter \`${param}\` type is expected as \`${expected}\` but provided as \`${received}\``,
    ),
  ];
  if (lines.length > 0) {
    return `${toolName} failed due to the following ${
      lines.length > 1 ? "issues" : "issue"
    }:\n${lines.join("\n")}`;
  }

  return (
    JSON.stringify(
      issues,
      (_key, value) => (typeof value === "bigint" ? value.toString() : value),
      2,
    ) ?? "[]"
  );
}

function projectInitialModelValidationIssues(
  inputSchema: ToolEntry["inputSchema"],
  jsonIssues: readonly ToolInputValidationIssue[],
  runtimeIssues: readonly RuntimeInputValidationIssue[] | undefined,
): ToolInputValidationIssue[] {
  if (!runtimeIssues || runtimeIssues.length === 0) return [...jsonIssues];

  const expandedRuntimeIssues = expandRuntimeIssues(runtimeIssues, jsonIssues);
  const matchedRuntimeIssues = new Set<number>();
  const matchedJsonIssues = new Map<number, ToolInputValidationIssue>();
  const jsonProjection: ToolInputValidationIssue[] = [];
  const jsonProjectionIdentities = new Set<string>();

// Runtime issues determine whether the error actually exists. JSON issues provide the target field structure.
// Establish a one-to-one mapping first to avoid directly splicing two sets of ordered lists back and forth and destroying the field verification sequence.
  for (const jsonIssue of jsonIssues) {
    const matchingRuntimeIndex = expandedRuntimeIssues.findIndex(
      (runtimeIssue, index) =>
        !matchedRuntimeIssues.has(index) && runtimeIssueMatchesJsonIssue(runtimeIssue, jsonIssue),
    );
    if (matchingRuntimeIndex < 0 && !isProviderOnlyStructuralIssue(jsonIssue, inputSchema)) {
      continue;
    }
    if (matchingRuntimeIndex >= 0) {
      matchedRuntimeIssues.add(matchingRuntimeIndex);
      matchedJsonIssues.set(matchingRuntimeIndex, jsonIssue);
    }
    const identity = toolInputIssueIdentity(jsonIssue);
    if (jsonProjectionIdentities.has(identity)) continue;
    jsonProjectionIdentities.add(identity);
    jsonProjection.push(jsonIssue);
  }

// After parameter type errors enter the formatter, all other issues will be omitted, so JSON projection is used directly
// Keep the target traversal order of missing / unexpected / wrong type.
  if (jsonProjection.some(isParameterFormatterIssue)) {
    return jsonProjection;
  }

  const runtimeProjection: ToolInputValidationIssue[] = [];
  const runtimeProjectionIdentities = new Set<string>();
  for (const [index, runtimeIssue] of expandedRuntimeIssues.entries()) {
    const issue =
      matchedJsonIssues.get(index) ?? canonicalizeRuntimeOnlyIssue(runtimeIssue, inputSchema);
    if (!issue) continue;
    const identity = toolInputIssueIdentity(issue);
    if (runtimeProjectionIdentities.has(identity)) continue;
    runtimeProjectionIdentities.add(identity);
    runtimeProjection.push(issue);
  }

// Non-standard runtime schema may only return success=false but no readable issue; in this case, it will be used
// The complete result of JSON Schema cannot reduce provider-visible error to an empty array.
  return runtimeProjection.length > 0
    ? orderRuntimeFallbackIssues(runtimeProjection)
    : [...jsonIssues];
}

function isParameterFormatterIssue(issue: ToolInputValidationIssue): boolean {
  return issue.code === "invalid_type" || issue.code === "unrecognized_keys";
}

function orderRuntimeFallbackIssues(
  issues: readonly ToolInputValidationIssue[],
): ToolInputValidationIssue[] {
  const ordered = [...issues];

// The local parser records array bounds before the array children are parsed; the target version is recorded after the children.
// Except for this one confirmed version difference, retain the original issue order of the runtime parser.
  for (let issueIndex = ordered.length - 1; issueIndex >= 0; issueIndex -= 1) {
    const issue = ordered[issueIndex];
    if (!issue || !isArrayBoundIssue(issue)) continue;

    let lastDescendantIndex = -1;
    for (
      let candidateIndex = ordered.length - 1;
      candidateIndex > issueIndex;
      candidateIndex -= 1
    ) {
      const candidate = ordered[candidateIndex];
      if (candidate && isStrictDescendantPath(candidate.path, issue.path)) {
        lastDescendantIndex = candidateIndex;
        break;
      }
    }
    if (lastDescendantIndex < 0) continue;

    ordered.splice(issueIndex, 1);
    ordered.splice(lastDescendantIndex, 0, issue);
  }

  return ordered;
}

function isArrayBoundIssue(
  issue: ToolInputValidationIssue,
): issue is Extract<ToolInputValidationIssue, { code: "too_big" | "too_small" }> {
  return (issue.code === "too_big" || issue.code === "too_small") && issue.origin === "array";
}

function isStrictDescendantPath(
  candidate: ToolInputValidationPath,
  parent: ToolInputValidationPath,
): boolean {
  return (
    candidate.length > parent.length &&
    parent.every((segment, index) => candidate[index] === segment)
  );
}

function expandRuntimeIssues(
  runtimeIssues: readonly RuntimeInputValidationIssue[],
  jsonIssues: readonly ToolInputValidationIssue[],
): RuntimeInputValidationIssue[] {
  const expanded: RuntimeInputValidationIssue[] = [];
  for (const issue of runtimeIssues) {
    const hasJsonUnion = jsonIssues.some((jsonIssue) =>
      runtimeIssueMatchesJsonIssue(issue, jsonIssue),
    );
    const unionIssues = hasJsonUnion ? [] : readRuntimeUnionIssues(issue);
    if (unionIssues.length > 0) {
      expanded.push(...expandRuntimeIssues(unionIssues, jsonIssues));
    } else {
      expanded.push(issue);
    }
  }
  return expanded;
}

function readRuntimeUnionIssues(issue: RuntimeInputValidationIssue): RuntimeInputValidationIssue[] {
  if (issue.code !== "invalid_union" || !Array.isArray(issue.unionErrors)) return [];

  return issue.unionErrors.flatMap((unionError) => {
    if (!isRecord(unionError) || !Array.isArray(unionError.issues)) return [];
    return unionError.issues.filter((nestedIssue): nestedIssue is RuntimeInputValidationIssue =>
      isRecord(nestedIssue),
    );
  });
}

function runtimeIssueMatchesJsonIssue(
  runtimeIssue: RuntimeInputValidationIssue,
  jsonIssue: ToolInputValidationIssue,
): boolean {
  const runtimePath = readRuntimeIssuePath(runtimeIssue);
  if (!runtimePath || !sameValidationPath(runtimePath, jsonIssue.path)) return false;

  const runtimeCode = normalizeRuntimeIssueCode(runtimeIssue.code);
  if (runtimeCode === jsonIssue.code) return true;

// Old parser uses invalid_type for missing enum/literal; target issue uses invalid_value.
  return (
    runtimeIssue.code === "invalid_type" &&
    runtimeIssue.received === "undefined" &&
    jsonIssue.code === "invalid_value"
  );
}

function canonicalizeRuntimeOnlyIssue(
  issue: RuntimeInputValidationIssue,
  inputSchema: ToolEntry["inputSchema"],
): ToolInputValidationIssue | undefined {
  const path = readRuntimeIssuePath(issue);
  if (!path || typeof issue.code !== "string") return undefined;

  switch (issue.code) {
    case "custom":
      return createCustomIssue(readRuntimeIssueMessage(issue), path);
    case "invalid_string":
      return canonicalizeRuntimeStringIssue(issue, path, inputSchema);
    case "too_big": {
      const origin = readRuntimeIssueOrigin(issue);
      return origin && typeof issue.maximum === "number"
        ? createTooBigIssue(origin, issue.maximum, path, {
            exact: issue.exact === true,
            inclusive: issue.inclusive !== false,
          })
        : undefined;
    }
    case "too_small": {
      const origin = readRuntimeIssueOrigin(issue);
      return origin && typeof issue.minimum === "number"
        ? createTooSmallIssue(origin, issue.minimum, path, {
            exact: issue.exact === true,
            inclusive: issue.inclusive !== false,
          })
        : undefined;
    }
    default:
      return undefined;
  }
}

function canonicalizeRuntimeStringIssue(
  issue: RuntimeInputValidationIssue,
  path: ToolInputValidationPath,
  inputSchema: ToolEntry["inputSchema"],
): ToolInputValidationIssue | undefined {
  if (issue.validation === "url") {
    const message =
      issue.message === "Invalid url" ? "Invalid URL" : readRuntimeIssueMessage(issue);
    return createInvalidFormatIssue("url", message, path);
  }
  if (issue.validation !== "regex") return undefined;

  const schemaNode = readSchemaAtPath(inputSchema, path);
  const pattern = typeof schemaNode?.pattern === "string" ? `/${schemaNode.pattern}/` : undefined;
  const message =
    issue.message === "Invalid" && pattern
      ? `Invalid string: must match pattern ${pattern}`
      : readRuntimeIssueMessage(issue);
  return createInvalidFormatIssue("regex", message, path, {
    origin: "string",
    pattern,
  });
}

function isProviderOnlyStructuralIssue(
  issue: ToolInputValidationIssue,
  inputSchema: ToolEntry["inputSchema"],
): boolean {
  if (issue.code === "unrecognized_keys") return true;
  if (!isMissingTypeIssue(issue)) return false;
  const schemaNode = readSchemaAtPath(inputSchema, issue.path);
  return !schemaNode || !Object.prototype.hasOwnProperty.call(schemaNode, "default");
}

function isMissingTypeIssue(
  issue: ToolInputValidationIssue,
): issue is Extract<ToolInputValidationIssue, { code: "invalid_type" }> {
  return issue.code === "invalid_type" && issue.message.includes("received undefined");
}

function readRuntimeIssueOrigin(
  issue: RuntimeInputValidationIssue,
): "array" | "number" | "string" | undefined {
  return issue.type === "array" || issue.type === "number" || issue.type === "string"
    ? issue.type
    : undefined;
}

function readRuntimeIssueMessage(issue: RuntimeInputValidationIssue): string {
  return typeof issue.message === "string" ? issue.message : "Invalid input";
}

function readRuntimeIssuePath(
  issue: RuntimeInputValidationIssue,
): ToolInputValidationPath | undefined {
  if (
    !Array.isArray(issue.path) ||
    !issue.path.every((segment) => typeof segment === "string" || typeof segment === "number")
  ) {
    return undefined;
  }
  return [...issue.path];
}

function normalizeRuntimeIssueCode(code: unknown): string | undefined {
  if (code === "invalid_enum_value" || code === "invalid_literal") return "invalid_value";
  if (code === "invalid_string") return "invalid_format";
  return typeof code === "string" ? code : undefined;
}

function sameValidationPath(
  left: ToolInputValidationPath,
  right: ToolInputValidationPath,
): boolean {
  return left.length === right.length && left.every((segment, index) => segment === right[index]);
}

function toolInputIssueIdentity(issue: ToolInputValidationIssue): string {
  return `${issue.code}:${JSON.stringify(issue.path)}:${issue.message}`;
}

function readSchemaAtPath(
  schema: ToolEntry["inputSchema"],
  path: ToolInputValidationPath,
): Record<string, unknown> | undefined {
  let current: unknown = schema;
  for (const segment of path) {
    if (!isRecord(current)) return undefined;
    if (typeof segment === "number") {
      current = current.items;
      continue;
    }
    const properties = current.properties;
    if (!isRecord(properties)) return undefined;
    current = properties[segment];
  }
  return isRecord(current) ? current : undefined;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function formatValidationPath(path: ToolInputValidationPath): string {
  if (path.length === 0) return "";
  return path.reduce<string>((formatted, segment, index) => {
    if (typeof segment === "number") return `${formatted}[${segment.toString()}]`;
    return index === 0 ? segment : `${formatted}.${segment}`;
  }, "");
}
