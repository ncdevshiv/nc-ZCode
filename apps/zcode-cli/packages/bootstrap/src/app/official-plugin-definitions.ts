import { ZCODE_OFFICIAL_PLUGIN_MARKETPLACE } from "@zcode/contracts";

//Built-in plug-in store information seed (written as it is in the official marketplace.json entry raw, key name and CDN directory
// Schema consistent: displayName_i18n / examplePrompts_i18n, etc.), parsing and reusing adapter
// parseEntryStoreListing. The icon points to the official assets CDN; the UI will safely downgrade to the default icon when the request fails.
export interface OfficialPluginListingSeed {
  displayName?: string;
  displayName_i18n?: Record<string, string>;
  description_i18n?: Record<string, string>;
  category?: string;
  author?: { name: string; url?: string };
  icon?: string;
  homepage?: string;
  privacyPolicy?: string;
  termsOfService?: string;
  heroImage?: string;
  examplePrompts?: string[];
  examplePrompts_i18n?: Record<string, string[]>;
}

const OFFICIAL_BROWSER_USE_PLUGIN_NAME = "browser-use";
export const OFFICIAL_BROWSER_USE_PLUGIN_ID = `${OFFICIAL_BROWSER_USE_PLUGIN_NAME}@${ZCODE_OFFICIAL_PLUGIN_MARKETPLACE}`;
/**
* node_repl host. It is not a user-oriented plug-in: it has no skills, no listings, and does not enter the market. Its only responsibility is
* Carry `dist/mcp/server.js`, a runtime product shared by Browser Use and Computer Use.
 *
* Why it needs to be a seed unit: The host product used to grow in the browser-use package, so
* resolveBuiltInNodeReplMcpServers can only be found under the rootPath of browser-use —— browser-use
* When the package is missing, the host cannot be obtained even if Computer Use is enabled by itself. After making an independent seed unit, the two plug-ins
* Each one only contributes its own domain assets, and whoever activates it can get the same host.
 */
export const OFFICIAL_NODE_REPL_HOST_PLUGIN_NAME = "node-repl-host";
export const OFFICIAL_NODE_REPL_HOST_PLUGIN_ID = `${OFFICIAL_NODE_REPL_HOST_PLUGIN_NAME}@${ZCODE_OFFICIAL_PLUGIN_MARKETPLACE}`;
const OFFICIAL_CUA_PLUGIN_NAME = "computer-use";
export const OFFICIAL_CUA_PLUGIN_ID = `${OFFICIAL_CUA_PLUGIN_NAME}@${ZCODE_OFFICIAL_PLUGIN_MARKETPLACE}`;

export interface OfficialPluginDefinition {
// Content-based plugin (no MCP server / no system dependencies) can be set to true,
// This way the user can use `/skill <name>` for the first time without having to `zcode plugins enable` first.
// Default is false to maintain the original behavior of heavy-load plugins such as ios-simulator / android-emulator.
  defaultEnabled?: boolean;
  listing?: OfficialPluginListingSeed;
  /**
* An MCP server provided by the host for this official plug-in but not included in the plugin manifest.
* Only used for product ownership and settings page status display; the host identity is still retained during runtime.
   */
  hostMcpServerNames?: readonly string[];
  name: string;
/** filesystem/SEA seed Refuse to generate a incomplete official plug-in cache when any item is missing. */
  requiredSeedPaths?: readonly string[];
  rootCandidates: readonly string[];
  /** Extra top-level paths intentionally staged as plugin runtime assets. */
  runtimeTopLevelPaths?: readonly string[];
  version: string;
}

const ZAI_AUTHOR = { name: "Z.ai", url: "https://z.ai" } as const;
const OFFICIAL_PLUGIN_ASSETS_BASE_URL = "https://cdn-zcode.z.ai/zcode/official-plugin/assets";

const OFFICIAL_NODE_REPL_HOST_REQUIRED_SEED_PATHS = ["dist/mcp/server.js"] as const;

export const OFFICIAL_BROWSER_USE_REQUIRED_SEED_PATHS = [
  "docs/api.json",
  "docs/documents.json",
  "docs/overview.md",
// documents.json has registered recording lookup; if the text is not forcibly verified, incomplete plug-ins that cannot read the screen recording guide will be seeded.
  "docs/recording.md",
  "docs/workflow.md",
  "scripts/browser-client.mjs",
  "skills/control-browser/SKILL.md",
  "skills/web-gui-tester/SKILL.md",
] as const;

const OFFICIAL_CUA_REQUIRED_SEED_PATHS = [
  "docs/computer-use.md",
  "scripts/computer-use-client.mjs",
  "skills/computer-use/SKILL.md",
] as const;

// zcode-guide originally does not have requiredSeedPaths, and seed will silently install one when it loses files.
// There is no plugin for the /workflow command - the symptom is that the command does not exist, without any diagnostics. commands/ are pinned to the skill text.
const OFFICIAL_ZCODE_GUIDE_REQUIRED_SEED_PATHS = [
  "commands/workflow.md",
  "skills/dynamic-workflows/SKILL.md",
  "skills/dynamic-workflows/examples.md",
  "skills/dynamic-workflows/patterns.md",
] as const;

export const OFFICIAL_PLUGIN_DEFINITIONS: readonly OfficialPluginDefinition[] = [
  {
// No listing: The host does not enter the market and is not exposed to users. It must always be available because of node_repl's registration gate
// It is "enable either Browser Use or Computer Use", and the host itself does not participate in that judgment.
    //
// defaultEnabled here does not violate the "content-based plug-ins only" agreement (see the description of computer-use below):
// The agreement is to prevent "injecting the entire tool set and pulling up the Helper upon first startup", but the seed host does neither - whether the tool
// The entry into the model tool pool is determined by the start and stop of the two capability plug-ins, and the Helper is only started when the SDK calls it for the first time.
    defaultEnabled: true,
    name: OFFICIAL_NODE_REPL_HOST_PLUGIN_NAME,
    requiredSeedPaths: OFFICIAL_NODE_REPL_HOST_REQUIRED_SEED_PATHS,
    rootCandidates: [
      "packages/node-repl-host",
      "../node-repl-host",
      "../../node-repl-host",
      "../../../node-repl-host",
    ],
    version: "0.6.0",
  },
  {
    listing: {
      author: ZAI_AUTHOR,
      category: "developer-tools",
      displayName: "Android Emulator",
      displayName_i18n: { "zh-CN": "Android emulator" },
      icon: `${OFFICIAL_PLUGIN_ASSETS_BASE_URL}/android-emulator/icon.png`,
      description_i18n: {
        "zh-CN": "Provides Android development workflow and emulator automation capabilities.",
      },
    },
    name: "android-emulator",
    rootCandidates: [
      "packages/android-emulator-plugin",
      "../android-emulator-plugin",
      "../../android-emulator-plugin",
      "../../../android-emulator-plugin",
    ],
    version: "0.1.0",
  },
  {
// The manifest only declares the browser-use skill; the host node_repl MCP is injected independently, and the package also carries its server/client
// runtime assets. Skills and browser bridges that only control "when/how to use the built-in browser" are enabled by default.
    defaultEnabled: true,
    hostMcpServerNames: ["node_repl"],
    listing: {
      author: ZAI_AUTHOR,
      category: "productivity",
      displayName: "Browser Use",
      displayName_i18n: { "zh-CN": "Browser operations" },
      icon: `${OFFICIAL_PLUGIN_ASSETS_BASE_URL}/browser-use/icon.png`,
      description_i18n: {
        "zh-CN": "Operate the ZCode built-in browser to inspect web pages and verify interactions.",
      },
    },
    name: OFFICIAL_BROWSER_USE_PLUGIN_NAME,
    requiredSeedPaths: OFFICIAL_BROWSER_USE_REQUIRED_SEED_PATHS,
    rootCandidates: [
      "packages/browser-use-plugin",
      "../browser-use-plugin",
      "../../browser-use-plugin",
      "../../../browser-use-plugin",
    ],
// If the official seed version is omitted when the plug-in package/manifest is upgraded, the old cache directory will continue to be loaded.
// The three versions of package, manifest, and definition should be consistent to avoid the release content and installation version from bifurcating again.
    version: "0.5.1",
  },
  ...(
    [
      ["documents", "docx", "Documents", "Word document"],
      ["pdf", "pdf", "PDF", "PDF"],
      ["presentations", "pptx", "Presentations", "Presentation document"],
      ["spreadsheets", "xlsx", "Spreadsheets", "spreadsheet"],
    ] as const
  ).map(
    ([name, skill, displayName, chineseName]): OfficialPluginDefinition => ({
      defaultEnabled: true,
      listing: {
        author: ZAI_AUTHOR,
        category: "productivity",
        displayName,
        displayName_i18n: { "zh-CN": chineseName },
// Reuse the published document icon, and split the plug-in without relying on new CDN resources.
        icon: `${OFFICIAL_PLUGIN_ASSETS_BASE_URL}/document-skills/icon.png`,
        description_i18n: { "zh-CN": `Create, edit and review ${chineseName} (${skill.toUpperCase()}).` },
      },
      name,
      requiredSeedPaths: ["agents/visual-judge.md", `skills/${skill}/SKILL.md`],
      rootCandidates: [
        `packages/${name}-plugin`,
        `../${name}-plugin`,
        `../../${name}-plugin`,
        `../../../${name}-plugin`,
      ],
      version: "0.1.7",
    }),
  ),
  {
// The official image search capability of the original aggregation document plug-in is still used, only the independent switch is removed; the authentication is still injected by the official MCP adapter.
    defaultEnabled: true,
    listing: {
      author: ZAI_AUTHOR,
      category: "productivity",
      displayName: "Image Search",
      displayName_i18n: { "zh-CN": "Search pictures" },
      description_i18n: { "zh-CN": "Find illustrations and reference images." },
    },
    name: "image-search",
    requiredSeedPaths: [".mcp.json"],
    rootCandidates: [
      "packages/image-search-plugin",
      "../image-search-plugin",
      "../../image-search-plugin",
      "../../../image-search-plugin",
    ],
    version: "0.1.1",
  },
  {
    listing: {
      author: ZAI_AUTHOR,
      category: "developer-tools",
      displayName: "iOS Simulator",
      displayName_i18n: { "zh-CN": "iOS simulator" },
      icon: `${OFFICIAL_PLUGIN_ASSETS_BASE_URL}/ios-simulator/icon.png`,
      description_i18n: {
        "zh-CN": "Provides iOS development workflow and simulator automation capabilities.",
      },
    },
    name: "ios-simulator",
    rootCandidates: [
      "packages/ios-simulator-plugin",
      "../ios-simulator-plugin",
      "../../ios-simulator-plugin",
      "../../../ios-simulator-plugin",
    ],
    version: "0.1.0",
  },
  {
    listing: {
      author: ZAI_AUTHOR,
      category: "utilities",
      displayName: "Restore Legacy Sessions",
      displayName_i18n: { "zh-CN": "Restore old session" },
      icon: `${OFFICIAL_PLUGIN_ASSETS_BASE_URL}/restore-legacy-sessions/icon.png`,
      description_i18n: {
        "zh-CN": "Restore legacy sessions to ZCode tasks and session records.",
      },
    },
    name: "restore-legacy-sessions",
    rootCandidates: [
      "packages/restore-legacy-sessions-plugin",
      "../restore-legacy-sessions-plugin",
      "../../restore-legacy-sessions-plugin",
      "../../../restore-legacy-sessions-plugin",
    ],
    version: "0.1.0",
  },
  {
    defaultEnabled: true,
    name: "plugin-creator",
    version: "0.1.1",
    listing: {
      author: ZAI_AUTHOR,
      category: "utilities",
      displayName: "Plugin Creator",
// The creator uses the client's own icon and no longer borrows the remote image from skill-creator.
      displayName_i18n: { "zh-CN": "plugin creator" },
      description_i18n: {
        "zh-CN": "Develop and verify ZCode plug-ins, and complete local dev market installation, trial and update.",
      },
    },
    rootCandidates: [
      "packages/plugin-creator-plugin",
      "../plugin-creator-plugin",
      "../../plugin-creator-plugin",
      "../../../plugin-creator-plugin",
    ],
    requiredSeedPaths: [
      "skills/plugin-creator/SKILL.md",
      "skills/plugin-creator/scripts/create-basic-plugin.mjs",
      "skills/plugin-creator/scripts/marketplace-files.mjs",
      "skills/plugin-creator/scripts/upsert-dev-marketplace.mjs",
      "skills/plugin-creator/scripts/scaffold-files.mjs",
      "skills/plugin-creator/scripts/validate-plugin.mjs",
      "skills/plugin-creator/references/plugin-json-spec.md",
      "skills/plugin-creator/references/installing-and-updating.md",
    ],
  },
  {
    defaultEnabled: true,
    listing: {
      author: ZAI_AUTHOR,
      category: "utilities",
      displayName: "Skill Creator",
      displayName_i18n: { "zh-CN": "Skill Creator" },
      icon: `${OFFICIAL_PLUGIN_ASSETS_BASE_URL}/skill-creator/icon.png`,
      description_i18n: { "zh-CN": "Create, edit, and validate reusable ZCode skills." },
    },
    name: "skill-creator",
    rootCandidates: [
      "packages/skill-creator-plugin",
      "../skill-creator-plugin",
      "../../skill-creator-plugin",
      "../../../skill-creator-plugin",
    ],
    version: "0.1.0",
  },
  {
//Pure content plug-in (only commands + skills, no MCP / no system dependencies), enabled by default,
// Let the user/agent get the ZCode configuration guide, self-diagnosis skills and dynamic workflow writing guide out of the box.
    defaultEnabled: true,
    listing: {
      author: ZAI_AUTHOR,
      category: "utilities",
      displayName: "ZCode Guide",
      displayName_i18n: { "zh-CN": "ZCode User Guide" },
      icon: `${OFFICIAL_PLUGIN_ASSETS_BASE_URL}/zcode-guide/icon.png`,
      description_i18n: {
        "zh-CN": "Provides ZCode configuration guide with plugins, skills, MCP, commands and hook diagnostics.",
      },
      examplePrompts: [
        "How do I configure MCP servers in ZCode?",
        "Diagnose my current ZCode setup",
      ],
      examplePrompts_i18n: {
        "zh-CN": ["How to configure MCP server in ZCode?", "Help me diagnose my current ZCode configuration"],
      },
    },
    name: "zcode-guide",
    requiredSeedPaths: OFFICIAL_ZCODE_GUIDE_REQUIRED_SEED_PATHS,
    rootCandidates: [
      "packages/zcode-guide-plugin",
      "../zcode-guide-plugin",
      "../../zcode-guide-plugin",
      "../../../zcode-guide-plugin",
    ],
    version: "0.2.0",
  },
  {
// Product decision: Computer control fallback is turned off by default, and the user needs to explicitly turn it on on the settings page.
// Therefore defaultEnabled is not declared here - computer-use carries MCP server and system Helper dependencies.
// Turning on by default means that every new user will inject the entire tool set and start the Helper when they first start.
// The old convention of "defaultEnabled content-based plug-ins only" is restored to integrity.
// The judgment formula is enabledPlugins[id] ?? defaultEnabled: Users who have manually opened it on the settings page have already placed it.
// Explicit true, not affected by this default value change. When changing back to the default enabled, synchronization is required
// List of packages/shared/src/plugin-marketplaces.ts (bootstrap single test machine compares the two),
// Linkage semantics between isZCodeCuaInternalFeatureEnabled (packaging layer default true) and input box entrance hidden default value.
    name: "computer-use",
    hostMcpServerNames: ["node_repl"],
// The user's exposed name is unified as "Computer Use/Computer Control". The package name and producer repository still remain zcode-cua,
// To be compatible with the native Helper identity; EN describes the baseline and uses the manifest
// description, only zh-CN override is placed here; resolveLocalizedText falls back to manifest when en-US.
    listing: {
      author: ZAI_AUTHOR,
      category: "productivity",
      displayName: "Computer Use",
      displayName_i18n: { "zh-CN": "computer control" },
      description_i18n: {
        "zh-CN": "Automated desktop application: The intelligent agent drives the mouse, keyboard and interface elements to complete actual tasks on your behalf.",
      },
// After the plug-in is renamed computer-use, the CDN icon is still published in the zcode-cua directory; use the resource path to avoid 404.
      icon: `${OFFICIAL_PLUGIN_ASSETS_BASE_URL}/zcode-cua/icon.png`,
    },
    rootCandidates: [
      "packages/zcode-cua-plugin",
      "../zcode-cua-plugin",
      "../../zcode-cua-plugin",
      "../../../zcode-cua-plugin",
    ],
    requiredSeedPaths: OFFICIAL_CUA_REQUIRED_SEED_PATHS,
// The current CUA is an unavailable placeholder package, so there is no need to copy the native runtime; to avoid bringing old local dependencies into the cache.
    runtimeTopLevelPaths: [],
// The version here tracks the upstream zcode-cua runtime version to enable plug-in UI display, cache path,
// Marketplace entries are all aligned; specific versions are maintained by the atomic producer bump workflow.
    version: "0.6.3",
  },
];

// If defaultEnabled: true is marked in the official plugin definition, it should be spelled in the form of `<name>@<marketplace>`,
// Pass it transparently to the adapter so that it is enabled by default when the user does not configure it explicitly (only applicable to content-based plugins).
// Note: Any entry point for resolving plugins (CLI subcommand resolveZCodePlugins, application startup resolveStartupPlugins)
// This collection must be passed to discoverNodePluginsSync, otherwise defaultEnabled will not take effect.
export const DEFAULT_ENABLED_OFFICIAL_PLUGIN_IDS: ReadonlySet<string> = new Set(
  OFFICIAL_PLUGIN_DEFINITIONS.filter((definition) => definition.defaultEnabled).map(
    (definition) => `${definition.name}@${ZCODE_OFFICIAL_PLUGIN_MARKETPLACE}`,
  ),
);

export function resolveOfficialPluginHostMcpServerNames(pluginId: string): string[] {
  const definition = OFFICIAL_PLUGIN_DEFINITIONS.find(
    (candidate) => `${candidate.name}@${ZCODE_OFFICIAL_PLUGIN_MARKETPLACE}` === pluginId,
  );
  return definition?.hostMcpServerNames ? [...definition.hostMcpServerNames] : [];
}

/**
* The official plug-in MCP injected by the host CLI (such as browser-use's `node_repl`) server name does not have the `plugin:` prefix,
* When the resource manager belongs to a plug-in, you need to check the official plug-in name.
 */
export function resolveOfficialPluginNameByHostMcpServerName(
  serverName: string,
): string | undefined {
  return OFFICIAL_PLUGIN_DEFINITIONS.find((definition) =>
    definition.hostMcpServerNames?.includes(serverName),
  )?.name;
}
