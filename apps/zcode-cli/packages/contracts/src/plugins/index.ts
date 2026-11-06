import type { CustomCommandRoot } from "../commands/index.js";
import type { HookEventName, HookMatcherConfig } from "../hooks/index.js";
import type { McpServerConfig } from "../interfaces/mcp.port.js";
import type { SkillRoot } from "../skills/index.js";
import type { ExecutionContext, TraceContext } from "../tracing/tracer.js";

export const ZCODE_OFFICIAL_PLUGIN_MARKETPLACE = "zcode-plugins-official";
export const ZCODE_INLINE_PLUGIN_MARKETPLACE = "inline";
export const ZCODE_PLUGIN_HOST_COMMAND = "__zcode-plugin-host";
/**
* Hide subcommand: sandbox process entry of dynamic workflow (`__zcode-dwf-child <entry path>`; the last bit of argv is
* Harness is the written entry file path, payload is only the command line).
 *
* Same family and mechanism as {@link ZCODE_PLUGIN_HOST_COMMAND}: SEA single-file binary does not interpret Node CLI flags,
* So harness' default `node --max-old-space-size=... <entry>` spawn will hand over the flag to strict under SEA
* parseArgs and must fail. Under SEA this binary is instead re-executed and dispatched by `run.ts` before parseArgs.
* Constants are placed in contracts instead of dynamic-workflow-runtime: the latter deliberately does not rely on contracts (app-free proof),
* The bootstrap passes it to the harness as argsPrefix after SEA determination.
 */
export const ZCODE_DWF_CHILD_COMMAND = "__zcode-dwf-child";

export function isOfficialMarketplaceId(id: string): boolean {
  return id === ZCODE_OFFICIAL_PLUGIN_MARKETPLACE;
}

export type PluginSource = "official" | "inline" | "cache";
export type PluginDiagnosticSeverity = "warning" | "error";

export type PluginDiagnosticCode =
  | "plugin_root_not_found"
  | "plugin_manifest_not_found"
  | "plugin_manifest_invalid"
  | "plugin_component_path_invalid"
  | "plugin_unsupported_component"
  | "plugin_skill_root_empty"
  | "plugin_mcp_read_failed"
  | "plugin_mcp_invalid"
  | "plugin_mcp_server_disabled"
  | "plugin_hook_read_failed"
  | "plugin_hook_invalid"
  | "plugin_hook_unsupported_event"
  | "plugin_dependency_invalid"
  | "plugin_dependency_missing"
  | "plugin_dependency_cycle"
  | "plugin_dependency_cross_marketplace"
  | "plugin_marketplace_invalid"
  | "plugin_marketplace_declaration_reserved"
  | "plugin_git_unavailable"
  | "plugin_archive_fetch_failed"
  | "plugin_marketplace_source_unsupported"
  | "plugin_validation_deferred"
  | "plugin_variable_missing"
  | "plugin_duplicate_id"
  | "plugin_not_found"
  | "plugin_ambiguous_name";

export interface PluginUserConfigOption {
  default?: string | number | boolean;
  description?: string;
  title?: string;
  required?: boolean;
  sensitive?: boolean;
  type?: "string" | "number" | "boolean" | "directory" | "file";
}

export type PluginOptionValue = string | number | boolean;
export type PluginOptionValues = Record<string, PluginOptionValue>;

export type PluginMarketplaceSourceConfig =
  | { source: "url"; headers?: Record<string, string>; url: string }
  | { path?: string; ref?: string; repo: string; source: "github"; sparsePaths?: string[] }
  | { path?: string; ref?: string; source: "git"; sparsePaths?: string[]; url: string }
  | { package: string; source: "npm" }
  | { source: "file"; path: string }
  | { source: "directory"; path: string };

export interface PluginMarketplaceConfig {
  source: PluginMarketplaceSourceConfig;
}

export interface PluginHookDetail {
  args?: string[];
  async?: boolean;
  command: string;
  event: HookEventName;
  matcher?: string;
  runnable: boolean;
  shell?: true | string;
  sourcePath: string;
  statusMessage?: string;
  timeout?: number;
  timeoutMs?: number;
  type: "command" | "process";
}

/** Details UI component grouping type, the order is consistent with the display: agent / command / skill / hook / mcp. */
export type PluginComponentKind = "agent" | "command" | "skill" | "hook" | "mcp";

export interface PluginComponentItem {
  name: string;
/** Description from component frontmatter/manifest; omitted if missing, never forged. */
  description?: string;
}

export interface PluginComponentGroup {
  kind: PluginComponentKind;
  items: PluginComponentItem[];
}

/**
* Store information (Store Listing): Display metadata carried by market directory entries, describing "how to present it in the store",
* Does not affect plug-in functionality. All fields are optional. If missing, the UI will be processed according to the degradation matrix (letter avatar / hidden block / omitted information row).
 */
export interface PluginStoreListing {
/** Card/details display name, missing fallback plug-in name slug. */
  displayName?: string;
  displayNameI18n?: Record<string, string>;
/** Multi-language version of the directory entry description (the description ontology already has a separate field). */
  descriptionI18n?: Record<string, string>;
/** icon image: https URL or local resource path of the built-in plug-in. */
  icon?: string;
  category?: string;
  author?: string;
  authorUrl?: string;
  homepage?: string;
  privacyPolicy?: string;
  termsOfService?: string;
/** Details page hero banner image. */
  heroImage?: string;
/** Detail page example prompt word capsule; pre-filled to create a new session after clicking. */
  examplePrompts?: string[];
  examplePromptsI18n?: Record<string, string[]>;
  /**
* A paid package is required to use: catalog entry declaration `requiresPaidPlan: true`, store card and details page
* A prompt icon is displayed on the right side of the title. Expressing "conditions of use" does not mean that the plug-in itself is a paid product.
* Therefore, it does not participate in the installation of access control and billing; the naming is not bound to the specific package product name, and renaming the package will not cause the field to expire.
   */
  requiresPaidPlan?: boolean;
}

export interface PluginManifest {
  agents?: unknown;
  author?: unknown;
  channels?: unknown;
  commands?: unknown;
  dependencies?: unknown;
  description?: string;
  homepage?: string;
  hooks?: unknown;
  keywords?: unknown;
  license?: string;
  lspServers?: unknown;
  mcpServers?: unknown;
  name: string;
  outputStyles?: unknown;
  repository?: string;
  settings?: unknown;
  skills?: unknown;
  userConfig?: Record<string, PluginUserConfigOption>;
  version?: string;
}

export interface PluginConfig {
  dirs: string[];
  enabled: boolean;
  enabledPlugins: Record<string, boolean>;
  extraKnownMarketplaces: Record<string, PluginMarketplaceConfig>;
  options: Record<string, PluginOptionValues>;
  suppressedBuiltins: string[];
}

export interface PluginMetadata {
/** The author name in the manifest (plugin.json) is normalized into a string; when the store information is missing, it will be returned to the details page. */
  author?: string;
  authorUrl?: string;
  commandRootCount: number;
  /**
* Authoritative component list (name + optional description), obtained by loader enumeration of the plugin root directory during the parsing phase,
* Has nothing to do with enabled status. Details are displayed directly in the UI, and there is no need to join Skills/Commands/Agents on the UI side.
   */
  components: PluginComponentGroup[];
  configuredOptions?: PluginOptionValues;
  dataPath: string;
  declaredMcpServerNames: string[];
  description?: string;
  enabled: boolean;
/** manifest (plugin.json) homepage; when store information is missing, it will fall back to the details page. */
  homepage?: string;
  id: string;
  manifestPath: string;
  marketplace: string;
  mcpServerNames: string[];
  name: string;
  hookDetails: PluginHookDetail[];
  rootPath: string;
  skillCount: number;
  skillRootCount: number;
  source: PluginSource;
  userConfig?: Record<string, PluginUserConfigOption>;
  version?: string;
}

export interface PluginDiagnostic {
  code: PluginDiagnosticCode;
  message: string;
  path?: string;
  pluginId?: string;
  severity: PluginDiagnosticSeverity;
}

// ============================================================
// The identity catalog contract of the Plugin conversation reference (@Plugin capability hint).
// Semantics: catalog is frozen when Session (App) is created,
// Only carries identity and capability "statements"; the actual injected capability intersects with the live inventory in each round.
// ============================================================

export interface PluginReferenceCatalogEntry {
/** Stable Plugin ID: `${manifest.name}@${marketplace}`, the unique identity of the canonical plugin:// link. */
  pluginId: string;
/** Manifest name (the base of the Skill/MCP/Subagent runtime namespace) is only used to display matching with provenance and is not authoritative. */
  name: string;
  marketplace: string;
/** The enabled state of catalog at the time of freezing; disabled entries are reserved for `disabled_in_session` diagnostics and cannot be referenced. */
  enabled: boolean;
  /**
* Other enabled Plugin stable IDs that share manifest.name with this entry.
* If it is not empty, it means V1 fail closed. Conflict: Picker is disabled, runtime is skipped by ambiguous,
* Disable last-write-wins or display name guessing.
   */
  conflictingPluginIds: string[];
/** Skill qualified names for identity claims (`${name}:${skill}`); derived from component enumeration, decoupled from live discovery. */
  skillQualifiedNames: string[];
/** Namespaced MCP server names for identity claims (`plugin:${name}:${server}`). */
  mcpServerNames: string[];
/** Canonical Subagent names for identity claims (`${name}:${agent}`); from component enumeration. */
  subagentNames: string[];
  /**
* Plugin root directory, only for runtime to perform provenance traceback (rootPath prefix determination) on live Skill/Subagent.
* No access to provider reminders or protocol projections - paths do not belong to identifiers-only contracts.
   */
  rootPath: string;
}

export interface PluginReferenceCatalog {
  plugins: PluginReferenceCatalogEntry[];
}

export interface PluginLoadOutcome {
  commandRoots: CustomCommandRoot[];
  diagnostics: PluginDiagnostic[];
  hooks: Partial<Record<HookEventName, HookMatcherConfig[]>>;
  mcpServers: Record<string, McpServerConfig>;
/** Store listings are associated according to the complete Plugin ID for CLI/TUI display; they do not participate in runtime identity judgment. */
  pluginListingsById?: Record<string, PluginStoreListing>;
  plugins: PluginMetadata[];
  skillRoots: SkillRoot[];
}

export interface PluginDiscoverRequest {
  config: PluginConfig;
  env?: Record<string, string | undefined>;
// bootstrap can pass in the official plugin id list of "safe enough to be enabled by default",
// Allow users to use it without `zcode plugins enable` first (such as content-only skill-creator).
// The default is an empty collection, and the behavior of existing plugins (including heavy loads such as ios-simulator/android-emulator) remains unchanged.
  officialPluginsEnabledByDefault?: ReadonlySet<string>;
  officialPluginRoots?: string[];
  storageRoot: string;
  trace?: TraceContext;
  workingDirectory: string;
}

export interface PluginOperationOptions {
  context?: ExecutionContext;
  signal?: AbortSignal;
}

export interface PluginPort {
  discoverPlugins(
    request: PluginDiscoverRequest,
    options?: PluginOperationOptions,
  ): Promise<PluginLoadOutcome>;
}
