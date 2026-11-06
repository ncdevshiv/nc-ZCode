import { realpathSync } from "node:fs";
import { platform as currentPlatform } from "node:process";
import type { ExecutionResult } from "@zcode/contracts";
import {
  getToolPathApi,
  normalizeToolPathForComparison,
  type ToolPathApi,
  type ToolPathPlatform,
} from "../path-normalization.js";
import type { ToolRuntimeScope } from "../types.js";

interface BashCwdPolicyInput {
  resolvedCwd: string | undefined;
  status: ExecutionResult["status"];
  exitCode: number | undefined;
  platform?: ToolPathPlatform;
  workspaceRoot: string;
  runtimeScope?: ToolRuntimeScope;
}

interface BashCwdPolicyDecision {
  nextWorkingDirectory?: string;
  stderrSuffix?: string;
}

interface NormalizedCwdBoundary {
  pathApi: ToolPathApi;
  resolvedCwd: string;
  workspaceRoot: string;
  equivalentPairs: Array<{ resolvedCwd: string; workspaceRoot: string }>;
}

export function decideBashCwdPolicy(input: BashCwdPolicyInput): BashCwdPolicyDecision {
  if (input.status !== "completed") return {};
  if (input.exitCode !== 0) return {};
  if (!input.resolvedCwd) return {};
  if ((input.runtimeScope ?? "main") !== "main") return {};

  const boundary = normalizeCwdBoundaryInput(input);

  if (isResolvedCwdInsideWorkspace(boundary)) {
    return { nextWorkingDirectory: boundary.resolvedCwd };
  }

  return {
    nextWorkingDirectory: boundary.workspaceRoot,
    stderrSuffix: `Shell cwd was reset to ${boundary.workspaceRoot}`,
  };
}

export function appendBashCwdStderrSuffix(stderr: string, suffix: string | undefined): string {
  if (!suffix) return stderr;
  const stderrWithoutTrailingNewlines = stderr.replace(/[\r\n]+$/, "");
  if (!stderrWithoutTrailingNewlines) return suffix;
  return `${stderrWithoutTrailingNewlines}\n${suffix}`;
}

function normalizeCwdBoundaryInput(input: BashCwdPolicyInput): NormalizedCwdBoundary {
  const platform = input.platform ?? currentPlatform;
  const pathApi = getToolPathApi(platform);
  const isWindows = platform === "win32";
  const resolvedCwd = normalizeCwdBoundary(input.resolvedCwd ?? "", isWindows);
  const workspaceRoot = normalizeCwdBoundary(input.workspaceRoot, isWindows);
  const equivalentPairs = [{ resolvedCwd, workspaceRoot }];
// cwd capture uses pwd -P/realpath to get the physical path; the session root may be symlink.
// Unify the collection of equivalent boundaries before making a judgment to avoid misjudgment of the physical path within the project as leaving the project.
  const realResolvedCwd = realpathCwdBoundary(input.resolvedCwd ?? "", isWindows);
  const realWorkspaceRoot = realpathCwdBoundary(input.workspaceRoot, isWindows);

  if (realResolvedCwd && realWorkspaceRoot) {
    equivalentPairs.push({
      resolvedCwd: realResolvedCwd,
      workspaceRoot: realWorkspaceRoot,
    });
  }

  return {
    pathApi,
    resolvedCwd,
    workspaceRoot,
    equivalentPairs,
  };
}

function isResolvedCwdInsideWorkspace(boundary: NormalizedCwdBoundary): boolean {
  return boundary.equivalentPairs.some(({ resolvedCwd, workspaceRoot }) =>
    isInsideOrSamePath(resolvedCwd, workspaceRoot, boundary.pathApi),
  );
}

function isInsideOrSamePath(child: string, parent: string, pathApi: ToolPathApi): boolean {
  const relativePath = pathApi.relative(parent, child);
  return relativePath === "" || !isPathOutsideParent(relativePath, pathApi);
}

function isPathOutsideParent(relativePath: string, pathApi: ToolPathApi): boolean {
  return (
    relativePath === ".." ||
    relativePath.startsWith(`..${pathApi.sep}`) ||
    pathApi.isAbsolute(relativePath)
  );
}

function normalizeCwdBoundary(value: string, isWindows: boolean): string {
  let normalized = normalizeToolPathForComparison(value, isWindows ? "win32" : "linux");
  if (!isWindows) {
    normalized = normalized
      .replace(/^\/private\/var\//, "/var/")
      .replace(/^\/private\/tmp(\/|$)/, "/tmp$1");
  }
  return normalized;
}

function realpathCwdBoundary(value: string, isWindows: boolean): string | undefined {
  try {
    return normalizeCwdBoundary(realpathSync.native(value), isWindows);
  } catch {
    return undefined;
  }
}
