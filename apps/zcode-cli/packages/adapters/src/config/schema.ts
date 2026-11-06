/* eslint-disable max-lines -- zcode-cli configuration schema requires centralized maintenance of file parsing and provider inheritance. Splitting them will make it more difficult to align configuration semantics. */
import { z } from "zod";
import type { RuntimeConfigPatch } from "@zcode/contracts";

const stringRecordSchema = z.record(z.string(), z.string());
const unknownRecordSchema = z.record(z.string(), z.unknown());
const positiveNumberSchema = z.number().finite().positive();
const positiveIntegerSchema = z.number().int().positive();
const modelStreamSchema = z.object({
  idleTimeoutMs: positiveNumberSchema.optional(),
});

const permissionSchema = z.object({
  mode: z.enum(["plan", "build", "edit", "yolo", "auto"]).optional(),
  allowedTools: z.array(z.string()).optional(),
  disallowedTools: z.array(z.string()).optional(),
  autoApproveHighRisk: z.boolean().optional(),
  allowMediumRiskInAuto: z.boolean().optional(),
});

const storageSchema = z.object({
  dir: z.string().min(1).optional(),
  sessionDbPath: z.string().min(1).optional(),
});

const networkSchema = z.object({
  httpProxy: z.string().min(1).optional(),
  noProxy: z.string().min(1).optional(),
  caCertFile: z.string().min(1).optional(),
  timeout: positiveNumberSchema.optional(),
});

const featuresSchema = z.object({
  compact: z.boolean().optional(),
  rewind: z.boolean().optional(),
  subagent: z.boolean().optional(),
  memory: z.boolean().optional(),
  skill: z.boolean().optional(),
  mcp: z.boolean().optional(),
});

const memorySchema = z.object({
  use: z.boolean().optional(),
});

const mcpServerBaseSchema = {
// The settings page and MCP adapter already support protocol selection; missing this field in the configuration entry will discard the entire server due to strict verification.
  protocolVersion: z.enum(["auto", "legacy", "2026-07-28"]).optional(),
  enabled: z.boolean().optional(),
  timeoutMs: positiveNumberSchema.optional(),
};

const mcpOAuthSchema = z.union([
  z
    .object({
      type: z.literal("client_credentials"),
      clientId: z.string().min(1),
      clientSecret: z.string().min(1),
      clientName: z.string().min(1).optional(),
      scope: z.string().optional(),
    })
    .strict(),
  z
    .object({
      type: z.literal("authorization_code"),
      clientId: z.string().min(1).optional(),
      clientSecret: z.string().min(1).optional(),
      clientName: z.string().min(1).optional(),
      redirectPath: z.string().min(1).optional(),
      scope: z.string().optional(),
    })
    .strict(),
]);

const mcpStdioServerSchema = z
  .object({
    ...mcpServerBaseSchema,
    type: z.literal("stdio"),
    command: z.string().min(1),
    args: z.array(z.string()).optional(),
    cwd: z.string().min(1).optional(),
    env: stringRecordSchema.optional(),
  })
  .strict();

const mcpHttpServerSchema = z
  .object({
    ...mcpServerBaseSchema,
    type: z.literal("http"),
    url: z.string().min(1),
    headers: stringRecordSchema.optional(),
    oauth: mcpOAuthSchema.optional(),
  })
  .strict();

const mcpSseServerSchema = z
  .object({
    ...mcpServerBaseSchema,
    type: z.literal("sse"),
    url: z.string().min(1),
    headers: stringRecordSchema.optional(),
    oauth: mcpOAuthSchema.optional(),
  })
  .strict();

const mcpServerSchema = z.preprocess(
  normalizeMcpServerConfigInput,
  z.discriminatedUnion("type", [mcpStdioServerSchema, mcpHttpServerSchema, mcpSseServerSchema]),
);

const mcpSchema = z.object({
  servers: z.record(z.string(), mcpServerSchema).optional(),
});

const pluginOptionValueSchema = z.union([z.string(), z.number(), z.boolean()]);

const pluginMarketplaceSourceSchema = z.discriminatedUnion("source", [
  z
    .object({
      source: z.literal("url"),
      url: z.string().min(1),
      headers: stringRecordSchema.optional(),
    })
    .strict(),
  z
    .object({
      source: z.literal("github"),
      repo: z.string().min(1),
      ref: z.string().min(1).optional(),
      path: z.string().min(1).optional(),
      sparsePaths: z.array(z.string().min(1)).optional(),
    })
    .strict(),
  z
    .object({
      source: z.literal("git"),
      url: z.string().min(1),
      ref: z.string().min(1).optional(),
      path: z.string().min(1).optional(),
      sparsePaths: z.array(z.string().min(1)).optional(),
    })
    .strict(),
  z
    .object({
      source: z.literal("npm"),
      package: z.string().min(1),
    })
    .strict(),
  z
    .object({
      source: z.literal("file"),
      path: z.string().min(1),
    })
    .strict(),
  z
    .object({
      source: z.literal("directory"),
      path: z.string().min(1),
    })
    .strict(),
]);

const pluginsSchema = z.object({
  enabled: z.boolean().optional(),
  dirs: z.array(z.string().min(1)).optional(),
  enabledPlugins: z.record(z.string(), z.boolean()).optional(),
  extraKnownMarketplaces: z
    .record(z.string().min(1), z.object({ source: pluginMarketplaceSourceSchema }).strict())
    .optional(),
  options: z.record(z.string(), z.record(z.string(), pluginOptionValueSchema)).optional(),
  suppressedBuiltins: z.array(z.string().min(1)).optional(),
});

export const LEGACY_CUA_PLUGIN_ID = "zcode-cua@zcode-plugins-official";
export const CANONICAL_CUA_PLUGIN_ID = "computer-use@zcode-plugins-official";

export function canonicalizePluginId(pluginId: string): string {
  return pluginId === LEGACY_CUA_PLUGIN_ID ? CANONICAL_CUA_PLUGIN_ID : pluginId;
}

export function pluginIdAliases(pluginId: string): readonly string[] {
  return canonicalizePluginId(pluginId) === CANONICAL_CUA_PLUGIN_ID
    ? [CANONICAL_CUA_PLUGIN_ID, LEGACY_CUA_PLUGIN_ID]
    : [pluginId];
}

const skillToggleSchema = z.object({
  enable: z.boolean().optional(),
});

// Skill/command availability coverage: key is the absolute path corresponding to .md, enable:false means disabled
const skillCommandOverrideSchema = z
  .object({
    enable: z.boolean().optional(),
  })
  .passthrough();
const skillCommandOverridesSchema = z.record(z.string(), skillCommandOverrideSchema);
const skillsSchema = z
  .object({
    enabled: z.boolean().optional(),
    includeInstructions: z.boolean().optional(),
    metadataBudget: positiveNumberSchema.optional(),
    roots: z.array(z.string()).optional(),
  })
  .catchall(skillToggleSchema);

const loggingSchema = z.object({
  level: z.enum(["debug", "info", "warn", "error"]).optional(),
  format: z.enum(["text", "json"]).optional(),
});

const uiSchema = z.object({
  locale: z.enum(["auto", "en-US", "zh-CN"]).optional(),
  theme: z.enum(["auto", "dark", "light"]).optional(),
});

const toolConcurrencySchema = z.object({
  maxConcurrency: positiveNumberSchema.optional(),
});

const modelAnomalyGuardSchema = z.object({
  toolCallWarningThreshold: positiveIntegerSchema.optional(),
  repeatedToolCallWarningThreshold: positiveIntegerSchema.optional(),
  maxBudgetWarningsPerTurn: z.number().int().nonnegative().optional(),
});

// Hooks schema：
//The ideal state is to re-export shared/workspace-hook-config, but the two pnpm workspaces resolve the physical
// Different zod instances (adapters 4.4.3 / shared 4.3.6): shared schema embeds this package combination schema
// Will make dts refer to the foreign zod internal type (TS2742), and the typeof/ZodType annotations will fall into a type cycle.
// Therefore, this copy is retained as is (the zod construct of this package) and remains equivalent to the verification semantics of shared;
// Runtime verification semantics are still based on shared (discovery/trust assembly entries all use shared schema——
// This schema is only responsible for configuration file loading diagnosis). If the zod instance is unified in the future, this copy should be deleted and re-exported.
const hookProcessSchema = z
  .object({
    type: z.literal("process"),
    command: z.string().min(1),
    enabled: z.boolean().optional(),
    args: z.array(z.string()).optional(),
    timeoutMs: positiveNumberSchema.optional(),
    statusMessage: z.string().min(1).optional(),
  })

  .passthrough();

const hookCommandSchema = z
  .object({
    type: z.literal("command"),
    command: z.string().min(1),
    enabled: z.boolean().optional(),
    async: z.boolean().optional(),
    shell: z.union([z.literal(true), z.string().min(1)]).optional(),
    timeout: positiveNumberSchema.optional(),
    timeoutMs: positiveNumberSchema.optional(),
    statusMessage: z.string().min(1).optional(),
  })
  .passthrough();

const hookMatcherSchema = z
  .object({
    matcher: z.string().min(1).optional(),
    hooks: z.array(z.discriminatedUnion("type", [hookProcessSchema, hookCommandSchema])).min(1),
  })
  .strict();

const hooksSchema = z
  .object({
    enabled: z.boolean().optional(),
    timeoutMs: positiveNumberSchema.optional(),
    maxOutputBytes: positiveNumberSchema.optional(),
    events: z
      .object({
        SessionStart: z.array(hookMatcherSchema).optional(),
        UserPromptSubmit: z.array(hookMatcherSchema).optional(),
        PreToolUse: z.array(hookMatcherSchema).optional(),
        PermissionRequest: z.array(hookMatcherSchema).optional(),
        PostToolUse: z.array(hookMatcherSchema).optional(),
        PostToolUseFailure: z.array(hookMatcherSchema).optional(),
        Stop: z.array(hookMatcherSchema).optional(),
      })
      .strict()
      .optional(),
  })
  .strict();

export const ZCodeConfigFileSchema = z
  .object({
    $schema: z.string().optional(),
    modelStream: modelStreamSchema.optional(),
    permission: permissionSchema.optional(),
    storage: storageSchema.optional(),
    network: networkSchema.optional(),
    features: featuresSchema.optional(),
    memory: memorySchema.optional(),
    mcp: mcpSchema.optional(),
    plugins: pluginsSchema.optional(),
    skills: skillsSchema.optional(),
    skill: skillCommandOverridesSchema.optional(),
    command: skillCommandOverridesSchema.optional(),
    logging: loggingSchema.optional(),
    ui: uiSchema.optional(),
    toolConcurrency: toolConcurrencySchema.optional(),
    modelAnomalyGuard: modelAnomalyGuardSchema.optional(),
    hooks: hooksSchema.optional(),
  })
  .passthrough();

export type ZCodeConfigFile = z.infer<typeof ZCodeConfigFileSchema>;

type SkillCommandOverrideMap = Record<string, { enable?: boolean }>;

export type ConfigDiagnosticSeverity = "warning" | "error";
export type ConfigDiagnosticCode =
  | "config_file_invalid"
  | "config_mcp_server_invalid"
  | "config_project_hooks_pending_trust";

export interface ConfigDiagnostic {
  code: ConfigDiagnosticCode;
  filePath?: string;
  message: string;
  path?: string;
  severity: ConfigDiagnosticSeverity;
}

interface ParseConfigFileResult {
  config: RuntimeConfigPatch;
  diagnostics: ConfigDiagnostic[];
}

function normalizeMcpServerConfigInput(value: unknown): unknown {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    return value;
  }

  const server = { ...(value as Record<string, unknown>) };
  if (!("env" in server) && "environment" in server) {
// The old configuration and some external imports use environment; the running state only consumes env.
// Normalize the parsing entry to prevent a legacy MCP server from bringing down the entire config.
    server.env = server.environment;
  }
  delete server.environment;

  if (typeof server.enable === "boolean" || typeof server.enabled === "boolean") {
//Historical legacy: In the early days of the desktop, the deactivation status was written as enable, while the contract field was always enabled.
// Desktop and mcp-sync now only write enabled, and when loading the configuration, the existing enable is migrated to disk;
// CLI can only read and parse, and cannot write to the disk. This only covers the window where "the user runs CLI first and has not opened the desktop yet".
// In case of conflict, "disable" shall prevail: writing enable:false on the desktop will not clean up the remaining enabled:true from external imports.
// Pressing the enabled value will restart the server that was turned off by the user (it is still being called after it has been disabled).
// After the stock configuration migration is completed, this section and the following delete can be deleted as a whole.
    server.enabled = server.enable === false ? false : (server.enabled ?? true);
  }
  delete server.enable;

  // External MCP configs may carry provider-specific timeout fields. ZCode does not migrate
  // those values, but the strict runtime schema should still accept old imported entries.
  delete server.timeout;
  delete server.startup_timeout_sec;

  if (server.type === "remote") {
// External Agent configuration often marks HTTP MCP as remote; ZCode running protocol type is http.
    server.type = "http";
  } else if (typeof server.type !== "string") {
// The app management regards the command form as the default stdio; the CLI also needs to infer the same,
// Otherwise, if the historical app configuration lacks type, it will block the loading of the model configuration.
    if (typeof server.command === "string" && server.command.trim().length > 0) {
      server.type = "stdio";
    } else if (typeof server.url === "string" && server.url.trim().length > 0) {
      server.type = "http";
    }
  }

  if (
    (server.type === "http" || server.type === "sse") &&
    server.headers === undefined &&
    server.http_headers !== undefined
  ) {
// Compatible with historical BigModel MCP configuration: the old field name is http_headers, and the agent runtime only consumes headers.
    server.headers = server.http_headers;
  }
  delete server.http_headers;

  return server;
}

export function parseConfigFileToRuntimePatchWithDiagnostics(
  value: unknown,
): ParseConfigFileResult {
  const diagnostics: ConfigDiagnostic[] = [];
  const normalized = normalizeConfigFileInput(value, diagnostics);
  const parsed = ZCodeConfigFileSchema.parse(normalized);
  return {
    config: parsedConfigFileToRuntimePatch(parsed),
    diagnostics,
  };
}

function parsedConfigFileToRuntimePatch(parsed: ZCodeConfigFile): RuntimeConfigPatch {
  const config: RuntimeConfigPatch = {};
  if (parsed.modelStream) config.modelStream = parsed.modelStream;

  if (parsed.permission) config.permission = parsed.permission;
  if (parsed.storage) config.storage = parsed.storage;
  if (parsed.network) config.network = parsed.network;
  if (parsed.features) config.features = parsed.features;
  if (parsed.memory) config.memory = parsed.memory;
  if (parsed.mcp) config.mcp = parsed.mcp;
  if (parsed.plugins) config.plugins = normalizePluginConfig(parsed.plugins);
  const skillsConfig = parseSkillsRuntimeConfig(parsed.skills);
  if (skillsConfig) config.skills = skillsConfig;
  const skillOverrides = mergeSkillCommandOverrides(
    parsed.skill,
    parseSkillOverridesFromPluralSkills(parsed.skills),
  );
  if (skillOverrides) config.skillOverrides = skillOverrides;
  if (parsed.command) config.commandOverrides = parsed.command;
  if (parsed.logging) config.logging = parsed.logging;
  if (parsed.ui) config.ui = parsed.ui;
  if (parsed.toolConcurrency) config.toolConcurrency = parsed.toolConcurrency;
  if (parsed.modelAnomalyGuard) config.modelAnomalyGuard = parsed.modelAnomalyGuard;
  if (parsed.hooks) config.hooks = parsed.hooks;

  return config;
}

function normalizePluginConfig(
  plugins: ZCodeConfigFile["plugins"],
): NonNullable<RuntimeConfigPatch["plugins"]> {
  if (!plugins) return {};
  const enabledPlugins = plugins.enabledPlugins ? { ...plugins.enabledPlugins } : undefined;
  if (enabledPlugins?.[LEGACY_CUA_PLUGIN_ID] !== undefined) {
    if (enabledPlugins[CANONICAL_CUA_PLUGIN_ID] === undefined) {
      enabledPlugins[CANONICAL_CUA_PLUGIN_ID] = enabledPlugins[LEGACY_CUA_PLUGIN_ID];
    }
    delete enabledPlugins[LEGACY_CUA_PLUGIN_ID];
  }
  const suppressedBuiltins = plugins.suppressedBuiltins
    ? plugins.suppressedBuiltins.reduce<string[]>((ids, id) => {
        const canonicalId = id === LEGACY_CUA_PLUGIN_ID ? CANONICAL_CUA_PLUGIN_ID : id;
        if (canonicalId === CANONICAL_CUA_PLUGIN_ID && ids.includes(CANONICAL_CUA_PLUGIN_ID)) {
          return ids;
        }
        ids.push(canonicalId);
        return ids;
      }, [])
    : undefined;
  const options = plugins.options ? { ...plugins.options } : undefined;
  if (options?.[LEGACY_CUA_PLUGIN_ID] !== undefined) {
    if (options[CANONICAL_CUA_PLUGIN_ID] === undefined) {
      options[CANONICAL_CUA_PLUGIN_ID] = options[LEGACY_CUA_PLUGIN_ID];
    }
    delete options[LEGACY_CUA_PLUGIN_ID];
  }
  return {
    ...plugins,
    ...(enabledPlugins ? { enabledPlugins } : {}),
    ...(suppressedBuiltins ? { suppressedBuiltins } : {}),
    ...(options ? { options } : {}),
  };
}

function normalizeConfigFileInput(value: unknown, diagnostics: ConfigDiagnostic[]): unknown {
  if (!isPlainRecord(value)) return value;

  const root = { ...value };
  const mcp = root.mcp;
  if (!isPlainRecord(mcp)) return root;

  const servers = mcp.servers;
  if (servers === undefined) return root;

  if (!isPlainRecord(servers)) {
    diagnostics.push({
      code: "config_mcp_server_invalid",
      message: "mcp.servers must be a JSON object; ignoring all MCP servers.",
      path: "mcp.servers",
      severity: "warning",
    });
    root.mcp = {
      ...mcp,
      servers: {},
    };
    return root;
  }

  const parsedServers: Record<string, unknown> = {};
  for (const [name, server] of Object.entries(servers)) {
    const parsed = mcpServerSchema.safeParse(server);
    if (parsed.success) {
      parsedServers[name] = parsed.data;
      continue;
    }

// MCP server is an optional tool configuration, and a single server error should not cause provider/plugin and other configurations to be lost.
    diagnostics.push({
      code: "config_mcp_server_invalid",
      message: formatMcpServerError(parsed.error),
      path: `mcp.servers.${name}`,
      severity: "warning",
    });
  }

  root.mcp = {
    ...mcp,
    servers: parsedServers,
  };
  return root;
}

function formatMcpServerError(error: z.ZodError): string {
  return error.issues
    .map((issue) => {
      const path = issue.path.length > 0 ? issue.path.join(".") : "<server>";
      return `${path}: ${issue.message}`;
    })
    .join("; ");
}

function isPlainRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function parseSkillsRuntimeConfig(
  skills: ZCodeConfigFile["skills"],
): RuntimeConfigPatch["skills"] | undefined {
  if (!skills) return undefined;
  const next: NonNullable<RuntimeConfigPatch["skills"]> = {};
  if (skills.enabled !== undefined) next.enabled = skills.enabled;
  if (skills.includeInstructions !== undefined) {
    next.includeInstructions = skills.includeInstructions;
  }
  if (skills.metadataBudget !== undefined) next.metadataBudget = skills.metadataBudget;
  if (skills.roots !== undefined) next.roots = skills.roots;
  return Object.keys(next).length > 0 ? next : undefined;
}

function parseSkillOverridesFromPluralSkills(
  skills: ZCodeConfigFile["skills"],
): SkillCommandOverrideMap | undefined {
  if (!skills) return undefined;
  const overrides: SkillCommandOverrideMap = {};
  for (const [path, value] of Object.entries(skills)) {
    if (!isAbsoluteConfigPath(path) || !isSkillCommandOverrideValue(value)) {
      continue;
    }
// The settings page currently writes a single skill switch to skills[SKILL.md absolute path].
// The running state of the agent only consumes skillOverrides, so the mapping is unified here during the configuration parsing stage to avoid bifurcation between the UI state and the actual loading of the agent.
    overrides[path] = { enable: value.enable };
  }
  return Object.keys(overrides).length > 0 ? overrides : undefined;
}

function mergeSkillCommandOverrides(
  ...overridesList: Array<SkillCommandOverrideMap | undefined>
): SkillCommandOverrideMap | undefined {
  const merged: SkillCommandOverrideMap = {};
  for (const overrides of overridesList) {
    if (!overrides) continue;
    Object.assign(merged, overrides);
  }
  return Object.keys(merged).length > 0 ? merged : undefined;
}

function isSkillCommandOverrideValue(value: unknown): value is { enable?: boolean } {
  return (
    typeof value === "object" &&
    value !== null &&
    !Array.isArray(value) &&
    ("enable" in value ? typeof (value as { enable?: unknown }).enable === "boolean" : true)
  );
}

function isAbsoluteConfigPath(path: string): boolean {
  return path.startsWith("/") || path.startsWith("\\") || /^[A-Za-z]:[\\/]/.test(path);
}
