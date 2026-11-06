import { createHash } from "node:crypto";
import type { CommandExecutionTelemetry, ToolExecutionTelemetry } from "@zcode/contracts";
import type { ToolExecutionContext } from "../types.js";
import { analyzeBashCommand } from "./bash-command-parser.js";
import { BASH_COMMAND_REGISTRY } from "./generated/bash-command-registry.js";

const COMMAND_HASH_LENGTH = 16;
const COMMAND_HASH_EDGE_CHARS = 4096;
const COMMAND_CLASSIFY_PREFIX_CHARS = 2048;
const MAX_COMMAND_IDENTITY_PARSE_CHARS = 8 * 1024;

export function roundNonNegativeMs(ms: number): number {
  if (!Number.isFinite(ms)) return 0;
  return Math.max(0, Math.round(ms));
}

export function elapsedMsSince(startedAt: number): number {
  return roundNonNegativeMs(Date.now() - startedAt);
}

export function commandHash(command: string): string {
  const hash = createHash("sha256");
  hash.update(String(command.length));
  hash.update("\0");
  hash.update(command.slice(0, COMMAND_HASH_EDGE_CHARS));
  if (command.length > COMMAND_HASH_EDGE_CHARS) {
    hash.update("\0");
    hash.update(command.slice(-COMMAND_HASH_EDGE_CHARS));
  }
  return hash.digest("hex").slice(0, COMMAND_HASH_LENGTH);
}

export function classifyCommand(command: string): string {
// Bugfix: Bash command may contain oversized heredoc/inline script. Telemetry classification only looks at bounded prefixes.
// Avoid copying and scanning the complete command to hide the point, and avoid misleading command classification by "npm test" in the text.
  const normalized = command.slice(0, COMMAND_CLASSIFY_PREFIX_CHARS).trimStart().toLowerCase();
  if (!normalized) return "empty";
  if (/\b(?:npm|pnpm|yarn|bun)\s+(?:test|run\s+test|vitest|jest)\b/u.test(normalized)) {
    return "test";
  }
  if (/\b(?:npm|pnpm|yarn|bun)\s+(?:install|add|update|remove)\b/u.test(normalized)) {
    return "package";
  }
  if (/\bgit\b/u.test(normalized)) return "git";
  if (/\b(?:rg|grep|find|fd)\b/u.test(normalized)) return "search";
  if (/\b(?:npm|pnpm|yarn|bun)\s+(?:run\s+)?(?:build|compile)\b/u.test(normalized)) {
    return "build";
  }
  if (/\b(?:curl|wget|gh\s+api)\b/u.test(normalized)) return "network";
  return "other";
}

export function classifySafeCommandIdentity(
  command: string,
): Pick<CommandExecutionTelemetry, "count" | "name"> {
// The category and hash are already bounded, but the safe command name will still be the complete heredoc/inline script
// Hand over to parser. Telemetry cannot make an extra O(n) CPU/memory for very large commands; it would rather be a count less at this point.
  if (command.length > MAX_COMMAND_IDENTITY_PARSE_CHARS) {
    return { name: "other" };
  }
  const analysis = analyzeBashCommand(command);
  const commandCount = analysis.commands.length;
  if (!command.trim()) return { count: 0, name: "empty" };
  if (analysis.hasParseErrors || analysis.hasUnsupportedSyntax || analysis.hasDynamicWords) {
    return { count: commandCount, name: "other" };
  }
  if (commandCount !== 1) {
    return {
      count: commandCount,
      name: commandCount > 1 ? "compound" : "other",
    };
  }
  const executable = basename(analysis.commands[0]?.name ?? "").toLowerCase();
  return {
    count: commandCount,
// Privacy boundary: Only static executable file names in the public command table are allowed to enter remote Trace;
// Custom script names and undetermined dynamic expressions are downgraded uniformly, and the original token is never uploaded.
    name: Object.hasOwn(BASH_COMMAND_REGISTRY, executable) ? executable : "other",
  };
}

export function workspaceKind(context: ToolExecutionContext): "local" | "remote" | "unknown" {
  return context.runtimeScope === "subagent" ? "unknown" : "local";
}

export function fileByteCount(content: string): number {
  return Buffer.byteLength(content, "utf8");
}

export function readToolExecutionTelemetry(
  output: unknown,
): ToolExecutionTelemetry | undefined {
  if (typeof output !== "object" || output === null || Array.isArray(output)) {
    return undefined;
  }
  const perf = (output as { perf?: unknown }).perf;
  if (typeof perf !== "object" || perf === null || Array.isArray(perf)) {
    return undefined;
  }
  return compactToolExecutionTelemetry(perf as ToolExecutionTelemetry);
}

export function compactToolExecutionTelemetry(
  perf: ToolExecutionTelemetry,
): ToolExecutionTelemetry | undefined {
  const compact: ToolExecutionTelemetry = {
    ...(perf.totalMs !== undefined ? { totalMs: perf.totalMs } : {}),
    ...(perf.permissionWaitMs !== undefined ? { permissionWaitMs: perf.permissionWaitMs } : {}),
    ...(perf.detail ? { detail: perf.detail } : {}),
  };
  return Object.keys(compact).length > 0 ? compact : undefined;
}

export function mergeToolExecutionTelemetry(
  ...items: (ToolExecutionTelemetry | undefined)[]
): ToolExecutionTelemetry | undefined {
  return compactToolExecutionTelemetry(Object.assign({}, ...items));
}

export function attachToolExecutionTelemetry<T extends object>(
  output: T,
  perf: ToolExecutionTelemetry | undefined,
): T {
  if (!perf) return output;
  Object.defineProperty(output, "perf", {
    configurable: true,
    enumerable: false,
    value: perf,
    writable: true,
  });
  return output;
}

function basename(value: string): string {
  return value.split(/[\\/]/u).at(-1) ?? "";
}
