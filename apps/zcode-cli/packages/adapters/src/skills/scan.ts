// ============================================================
//Skill root scanning rules (shared implementation)
// ============================================================

// The skills item in the manifest can point to either a single skill directory or a layer of skill collections.
// When the root directory contains SKILL.md, identify the root skill first, and then scan a layer of subdirectories; consistent with the desktop discovery rules.
// Avoid mistaking a single skill for a collection and silently missing the scan.
// The plug-in discovery link (countSkillFiles/collectSkillComponents) is a synchronous code, and the skill adapter goes
// fs/promises, therefore provides two variants of sync/async. The rules are only written once and followed by respective implementations.
//
// Error contract: only swallow "normal missing" - return an empty array when the path does not exist (ENOENT) or is not a directory; other errors
// (such as EACCES permissions, EMFILE) must be thrown upward, and the caller decides whether to send skill_scan_failed diagnosis or
// Graceful degradation. The outer catch cannot swallow all errors: permission errors will degenerate into silent empty, skill_scan_failed
// Diagnostics become dead code and silent failure paths are reintroduced.
//
// Trust boundary: plugin-scope content is not trusted, symbolic links (including Windows
// junction - Dirent.isSymbolicLink/lstat returns true for both) can point to anything outside the plugin root
// Directory or file (directory level skills/evil-link -> ../../outside, file level SKILL.md -> ~/.aws/
// credentials). Realpath containment was tried, but the boundaries were spread along the data stream as optional parameters, each
// Remember to verify file contact points (file-level links, cross-drive letter relative predicates, manifest failure fallback)
// notch). Converged to a single rule: plugin scans never follow symbolic links - root itself, subdirectory candidates,
// The SKILL.md file rejects links at all three granularities (rejection means no escape, no need to determine where the link points).
// User-level skill roots (symlink imports of ~/.zcode/skills are a supported feature) remain the default ones to follow.

import { lstatSync, readdirSync, statSync } from "node:fs";
import { lstat, readdir, stat } from "node:fs/promises";
import { join } from "node:path";
import { SKILL_FILE_NAME, shouldWalkSkillDirectoryEntry } from "@zcode/shared";

interface ScanSkillFilesOptions {
  /**
* Whether to follow symbolic links (directory level and file level), default true (symlink import of user-level skill root
* is a supported feature). Plugin-scope scanning must pass false: the link can point outside the plug-in root and is rejected.
* The link refuses to escape and does not require realpath to determine the direction.
   */
  followSymbolicLinks?: boolean;
}

/**
* Absolute paths of all real SKILL.md in the root directory (including the root itself).
* If the root directory does not exist or is not a directory, an empty array is returned; other errors during the scanning process (EACCES, etc.) are thrown upward.
 */
export async function scanSkillFilesUnderRoot(
  rootPath: string,
  options: ScanSkillFilesOptions = {},
): Promise<string[]> {
  const followSymlinks = options.followSymbolicLinks ?? true;
// When the root itself is a link (including junction), the plug-in scan directly determines that it is empty.
  if (!followSymlinks && (await isSymbolicLink(rootPath))) return [];

  const rootInfo = await stat(rootPath).catch((error) => {
    throwIfUnexpectedScanError(error);
    return null;
  });
  if (!rootInfo?.isDirectory()) return [];

  const files: string[] = [];
  const own = join(rootPath, SKILL_FILE_NAME);
// It is normal that there is no SKILL.md in the root itself. Continue to scan one layer of subdirectories.
  if (await isLoadableSkillFile(own, followSymlinks)) files.push(own);

  const entries = await readdir(rootPath, { withFileTypes: true });
  for (const entry of entries) {
    const walkable = entry.isDirectory() || (followSymlinks && entry.isSymbolicLink());
    if (!walkable) continue;
    if (!shouldWalkSkillDirectoryEntry(entry.name)) continue;
// The path hit by the subdirectory must verify that the file actually exists: classified directory (such as skills/engineering/)
// It is not a skill directory, and the spelled out path does not exist; parseSkill will skip it silently, but the count will be the same as
// Component enumeration consumes this result, and if it is not verified, "non-existent skills" will be included.
    const candidate = join(rootPath, entry.name, SKILL_FILE_NAME);
    if (await isLoadableSkillFile(candidate, followSymlinks)) files.push(candidate);
  }
  return files;
}

/** Synchronous variant of scanSkillFilesUnderRoot, same error contract. */
export function scanSkillFilesUnderRootSync(
  rootPath: string,
  options: ScanSkillFilesOptions = {},
): string[] {
  const followSymlinks = options.followSymbolicLinks ?? true;
  if (!followSymlinks && isSymbolicLinkSync(rootPath)) return [];

  const rootInfo = statSyncOrNull(rootPath);
  if (!rootInfo?.isDirectory()) return [];

  const files: string[] = [];
  const own = join(rootPath, SKILL_FILE_NAME);
  if (isLoadableSkillFileSync(own, followSymlinks)) files.push(own);

  for (const entry of readdirSync(rootPath, { withFileTypes: true })) {
    const walkable = entry.isDirectory() || (followSymlinks && entry.isSymbolicLink());
    if (!walkable) continue;
    if (!shouldWalkSkillDirectoryEntry(entry.name)) continue;
    const candidate = join(rootPath, entry.name, SKILL_FILE_NAME);
    if (isLoadableSkillFileSync(candidate, followSymlinks)) files.push(candidate);
  }
  return files;
}

/**
* SKILL.md candidate loadability determination: only real files will be included. Symlink file when not following link
* (including junction) are always rejected - this is the line of defense for file-level escape (SKILL.md -> external arbitrary files).
 */
async function isLoadableSkillFile(path: string, followSymlinks: boolean): Promise<boolean> {
  if (!followSymlinks && (await isSymbolicLink(path))) return false;
  const info = await stat(path).catch((error) => {
    throwIfUnexpectedScanError(error);
    return null;
  });
  return info?.isFile() ?? false;
}

function isLoadableSkillFileSync(path: string, followSymlinks: boolean): boolean {
  if (!followSymlinks && isSymbolicLinkSync(path)) return false;
  return statSyncOrNull(path)?.isFile() ?? false;
}

async function isSymbolicLink(path: string): Promise<boolean> {
  try {
    return (await lstat(path)).isSymbolicLink();
  } catch {
    return false;
  }
}

function isSymbolicLinkSync(path: string): boolean {
  try {
    return lstatSync(path).isSymbolicLink();
  } catch {
    return false;
  }
}

function statSyncOrNull(path: string): { isFile(): boolean; isDirectory(): boolean } | null {
  try {
    return statSync(path);
  } catch (error) {
    throwIfUnexpectedScanError(error);
    return null;
  }
}

/** ENOENT (path does not exist) is the normal state of scanning and is swallowed; other errors (EACCES, etc.) are thrown upward. */
function throwIfUnexpectedScanError(error: unknown): void {
  if (
    typeof error === "object" &&
    error !== null &&
    "code" in error &&
    (error as { code?: unknown }).code === "ENOENT"
  ) {
    return;
  }
  throw error;
}
