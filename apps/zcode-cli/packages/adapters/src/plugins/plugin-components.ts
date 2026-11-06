import { readdirSync } from "node:fs";
import { basename, dirname, join } from "node:path";
import type {
  PluginComponentGroup,
  PluginComponentItem,
  PluginComponentKind,
  PluginDiagnostic,
  PluginManifest,
} from "@zcode/contracts";
import { directoryExists, isRecord, resolveInside } from "./helpers.js";
import { listPluginHookEventNames } from "./hook-sources.js";
import { readMarkdownFrontmatter } from "./markdown-frontmatter.js";
import { loadPluginMcpServerDefinitions } from "./mcp.js";
import { scanSkillFilesUnderRootSync } from "../skills/scan.js";
import type { LoadedPlugin } from "./types.js";

// The component grouping type definition has been moved up to @zcode/contracts (PluginMetadata needs to be referenced), and is exported here to maintain the stability of the external contract.
export type { PluginComponentGroup, PluginComponentItem, PluginComponentKind };

/**
* Enumerate the names and descriptions of various components on the parsed plug-in root directory. Pure file reading, cross-platform only uses node:path/fs.
* The manifest is passed in after being read by the caller (can be null); the description is skipped or omitted when the directory is missing or there is no frontmatter.
* Do not block the entire enumeration due to a single component exception.
 */
export function enumeratePluginComponents(
  rootPath: string,
  manifest: PluginManifest | null,
  options: { diagnostics?: PluginDiagnostic[]; loaded?: LoadedPlugin } = {},
): PluginComponentGroup[] {
  const groups: PluginComponentGroup[] = [];
  const agentItems = collectMarkdownComponents(rootPath, manifest?.agents, "agents");
  if (agentItems.length > 0) groups.push({ kind: "agent", items: agentItems });

  const commandItems = collectMarkdownComponents(rootPath, manifest?.commands, "commands");
  if (commandItems.length > 0) groups.push({ kind: "command", items: commandItems });

// Trust boundary: The rootPath of component enumeration is the plug-in root. Scanning unconditionally does not follow symbolic links——
// Does not depend on whether loaded is complete (the marketplace describe link that fails to parse the manifest will also take effect).
  const skillItems = collectSkillComponents(rootPath, manifest?.skills);
  if (skillItems.length > 0) groups.push({ kind: "skill", items: skillItems });

  const hookItems = collectHookComponents(manifest, options);
  if (hookItems.length > 0) groups.push({ kind: "hook", items: hookItems });

  const mcpItems = collectMcpComponents(manifest, options);
  if (mcpItems.length > 0) groups.push({ kind: "mcp", items: mcpItems });

  return groups;
}

/** command/agent: the default directory + the .md file in the additional path declared by the manifest, read frontmatter name/description. */
function collectMarkdownComponents(
  rootPath: string,
  manifestField: unknown,
  defaultDir: "commands" | "agents",
): PluginComponentItem[] {
  const items: PluginComponentItem[] = [];
  const seen = new Set<string>();

// object form declaration ({ name: { source|content, description } }) directly takes the description of the declaration.
  if (isRecord(manifestField)) {
    for (const [rawName, rawMeta] of Object.entries(manifestField)) {
      const name = rawName.trim();
      if (!name || seen.has(name)) continue;
      seen.add(name);
      const description =
        isRecord(rawMeta) && typeof rawMeta.description === "string"
          ? rawMeta.description.trim()
          : undefined;
      items.push(description ? { name, description } : { name });
    }
  }

  const dirs = collectComponentDirs(rootPath, manifestField, defaultDir);
  for (const dir of dirs) {
    let entries: string[];
    try {
      entries = readdirSync(dir, { withFileTypes: true })
        .filter((dirent) => dirent.isFile() && dirent.name.endsWith(".md"))
        .map((dirent) => dirent.name);
    } catch {
      continue;
    }
    for (const fileName of entries) {
      const baseName = fileName.slice(0, -".md".length);
      const fm = readMarkdownFrontmatter(join(dir, fileName));
      const name = fm.name ?? baseName;
      if (seen.has(name)) continue;
      seen.add(name);
      items.push(fm.description ? { name, description: fm.description } : { name });
    }
  }
  return items;
}

/**
* Skill: Enumerate skills under the default skills directory and manifest declaration path, and read SKILL.md frontmatter.
* When the root itself contains SKILL.md, the root itself is a skill; remove duplication according to the file path and final name to avoid declaring the root and the default root
* Repeated counting also avoids the drift of display results caused by skills with the same name under different paths.
 */
function collectSkillComponents(rootPath: string, manifestField: unknown): PluginComponentItem[] {
  const items: PluginComponentItem[] = [];
  const seenFiles = new Set<string>();
  const seenNames = new Set<string>();
  const dirs = collectComponentDirs(rootPath, manifestField, "skills");
  for (const dir of dirs) {
    let skillFiles: string[];
    try {
      skillFiles = scanSkillFilesUnderRootSync(dir, { followSymbolicLinks: false });
    } catch {
      continue;
    }
    for (const skillFile of skillFiles) {
      if (seenFiles.has(skillFile)) continue;
      seenFiles.add(skillFile);
      const fallbackName = basename(dirname(skillFile));
      const fm = readMarkdownFrontmatter(skillFile);
      const name = fm.name ?? fallbackName;
      if (seenNames.has(name)) continue;
      seenNames.add(name);
      items.push(fm.description ? { name, description: fm.description } : { name });
    }
  }
  return items;
}

/** Hook: Reuse the source discovery rules of the loader, but only take the event name for detailed display, and do not construct an executable hook. */
function collectHookComponents(
  manifest: PluginManifest | null,
  options: { diagnostics?: PluginDiagnostic[]; loaded?: LoadedPlugin },
): PluginComponentItem[] {
  if (options.loaded) {
    return listPluginHookEventNames({
      diagnostics: options.diagnostics ?? [],
      loaded: options.loaded,
    }).map((name) => ({ name }));
  }
  if (!manifest) return [];
  return collectInlineHookEvents(manifest.hooks).map((name) => ({ name }));
}

/** mcp: Reuse loader to read `.mcp.json` + `manifest.mcpServers` pure read analysis, only display the original server name. */
function collectMcpComponents(
  manifest: PluginManifest | null,
  options: { diagnostics?: PluginDiagnostic[]; loaded?: LoadedPlugin },
): PluginComponentItem[] {
  if (options.loaded) {
    return Object.keys(
      loadPluginMcpServerDefinitions({
        diagnostics: options.diagnostics ?? [],
        loaded: options.loaded,
      }),
    )
      .map((name) => name.trim())
      .filter((name) => name.length > 0)
      .map((name) => ({ name }));
  }
  if (!manifest || !isRecord(manifest.mcpServers)) return [];
  return Object.keys(manifest.mcpServers)
    .map((name) => name.trim())
    .filter((name) => name.length > 0)
    .map((name) => ({ name }));
}

function collectInlineHookEvents(value: unknown): string[] {
  const hooksField =
    isRecord(value) && isRecord((value as Record<string, unknown>).hooks)
      ? ((value as Record<string, unknown>).hooks as Record<string, unknown>)
      : value;
  if (!isRecord(hooksField)) return [];
  const seen = new Set<string>();
  const names: string[] = [];
  for (const event of Object.keys(hooksField)) {
    const name = event.trim();
    if (!name || seen.has(name)) continue;
    seen.add(name);
    names.push(name);
  }
  return names;
}

/** Default directory convention + manifest string/array path declaration, combined into a deduplicated directory list to be scanned. */
function collectComponentDirs(rootPath: string, manifestField: unknown, defaultDir: string): string[] {
  const dirs: string[] = [];
  const seen = new Set<string>();
  const add = (raw: string): void => {
    const resolvedPath = resolveInside(rootPath, raw.replace(/^\.\//, ""));
    if (!resolvedPath || seen.has(resolvedPath)) return;
    seen.add(resolvedPath);
    dirs.push(resolvedPath);
  };
  const defaultPath = join(rootPath, defaultDir);
  if (directoryExists(defaultPath)) add(defaultDir);
  if (typeof manifestField === "string") add(manifestField);
  else if (Array.isArray(manifestField)) {
    for (const value of manifestField) if (typeof value === "string") add(value);
  }
  return dirs;
}
