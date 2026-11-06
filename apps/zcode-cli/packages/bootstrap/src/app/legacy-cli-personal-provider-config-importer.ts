import { readFile } from "node:fs/promises";
import { BUILTIN_PROVIDER_TEMPLATE_IDS } from "@zcode/shared";
import { getDefaultConfigPath } from "@zcode/adapters/config";
import {
  parseLegacyCliModelConfig,
  type LegacyCliModelConfigProjection,
} from "./legacy-cli-model-config.js";
import {
  ApiKeyAccessConfig,
  ModelConfig,
  ModelConfigRules,
  ModelPropertiesConfig,
  ProviderApiConfig,
  ProviderConfig,
  ProviderConfigMap,
  type ProviderApiType,
  type ProviderConfigLayerUpdate,
  type ModelSelection,
} from "@zcode/provider";

interface LegacyCliPersonalProviderConfigImportInput {
  readonly input: unknown;
}

const RETIRED_ZAPI_PROVIDER_ID = "builtin:zapi";

class UnsupportedLegacyCliProviderConfigError extends Error {
  readonly providerId: string;

  constructor(providerId: string) {
    super(`Old CLI Provider ${providerId} contains execution fields that formal Config cannot yet express`);
    this.name = "UnsupportedLegacyCliProviderConfigError";
    this.providerId = providerId;
  }
}

function resolveLegacyProviderApiType(
  kind: "anthropic" | "openai" | "openai-compatible" | undefined,
): ProviderApiType {
  switch (kind) {
    case "anthropic":
      return "anthropic-messages";
    case "openai":
      return "openai-responses";
    case "openai-compatible":
    case undefined:
      return "openai-chat-completions";
  }
  return "openai-chat-completions";
}

/** Migrate explicit Provider definitions in the old CLI user file to the new Personal Overlay. */
function importLegacyCliPersonalProviderConfig(
  input: LegacyCliPersonalProviderConfigImportInput,
): ProviderConfigLayerUpdate {
  const runtimePatch = parseLegacyCliModelConfig(input.input);
// The general ZCodeConfigFileSchema deliberately leaves the old provider in passthrough unknown.
// The migrator takes the type from the result, causing Bootstrap build to be unable to prove the model/provider structure.
// Here only consumes the verified projection of the dedicated old format parser, keeping the compatibility logic enclosed at the read boundary.
  let providers = ProviderConfigMap.empty();
  let models = ModelConfigRules.empty();

  for (const [rawProviderId, provider] of Object.entries(runtimePatch.provider ?? {})) {
    const providerId = rawProviderId.trim();
    if (!providerId) continue;
// Standalone also reads builtin:* / source=custom written by the old Desktop.
// Consistent with Desktop import: the old built-in static configuration and account credentials will not be migrated, and only the Key will be left in the pay-per-volume API.
    const templateId =
      providerId === "builtin:bigmodel"
        ? BUILTIN_PROVIDER_TEMPLATE_IDS.bigmodel
        : providerId === "builtin:zai"
          ? BUILTIN_PROVIDER_TEMPLATE_IDS.zai
          : undefined;
    if (templateId) {
      const apiKey = provider.options?.apiKey?.trim();
      if (apiKey)
        providers = providers.setRule({
          providerId: templateId,
          templateId,
          config: new ProviderConfig({
            group: "standard-personal",
            access: new ApiKeyAccessConfig({ apiKey }),
          }),
        });
      continue;
    }
    if (providerId.startsWith("builtin:") || providerId.startsWith("account:")) continue;
    if (provider.source !== undefined && provider.source !== "custom") continue;
    if (requiresUnsupportedNoAuthentication(provider)) {
      throw new UnsupportedLegacyCliProviderConfigError(providerId);
    }
    const members = collectLegacyCliModelMembers(provider);
    const modelIds = members.map((member) => member.modelId);
    const providerName = provider.name?.trim();
    providers = providers.setRule({
      providerId,
      providerName: providerName && providerName !== providerId ? providerName : undefined,
      config: new ProviderConfig({
        group: "standard-personal",
        access: new ApiKeyAccessConfig({
          apiKey: provider.options?.apiKey,
        }),
        api: new ProviderApiConfig({
          type: resolveLegacyProviderApiType(provider.kind),
          baseUrl: provider.options?.baseURL,
          headers: mergeHeaders(provider.headers, provider.options?.headers),
        }),
        ...(modelIds.length > 0 ? { personalModelIds: modelIds, modelOrder: modelIds } : {}),
      }),
    });
    for (const member of members) {
      if (member.contextWindow === undefined) continue;
      models = models.setExact(
        providerId,
        member.modelId,
        new ModelConfig({
          properties: new ModelPropertiesConfig({ contextWindow: member.contextWindow }),
        }),
      );
    }
  }

// The default selection is generated from the same old file read as the Provider, and is only imported when the Personal file is first created.
// For subsequent clearing, the old main cannot be read again by default, nor can half of the configuration be created first to prevent the remaining fields from being migrated.
  const defaultModelSelection = importLegacyCliConfiguredDefault(input.input) ?? undefined;
  return Object.freeze({ providers, models, defaultModelSelection });
}

export async function readLegacyCliPersonalProviderConfig(input: {
  readonly filePath?: string;
}): Promise<ProviderConfigLayerUpdate | null> {
  const filePath = input.filePath ?? getDefaultConfigPath();
  try {
    const raw = JSON.parse(await readFile(filePath, "utf8")) as unknown;
    return importLegacyCliPersonalProviderConfig({ input: raw });
  } catch (error) {
    if (isFileNotFound(error)) return null;
// Migration must be all or nothing. Preserve the old link when the formal Config cannot yet express the old execution fields,
// Not writing a partial Personal file prevents re-migration of future versions.
    if (error instanceof UnsupportedLegacyCliProviderConfigError) return null;
    throw error;
  }
}

/** The main model in the old CLI user files is only migrated to the Environment default selection. */
function importLegacyCliConfiguredDefault(input: unknown): ModelSelection | null {
  const main = parseLegacyCliModelConfig(input).model?.main;
  if (!main || main.provider === RETIRED_ZAPI_PROVIDER_ID) return null;
  return Object.freeze({
    providerId: main.provider,
    modelId: main.model,
  });
}

type LegacyCliProvider = NonNullable<LegacyCliModelConfigProjection["provider"]>[string];

interface LegacyCliModelMember {
  readonly modelId: string;
  readonly contextWindow?: number;
}

function collectLegacyCliModelMembers(
  provider: LegacyCliProvider,
): readonly LegacyCliModelMember[] {
  const seen = new Set<string>();
  const members: LegacyCliModelMember[] = [];
  for (const [modelKey, model] of Object.entries(provider.models ?? {})) {
    if (model.deleted === true) continue;
    const modelId = (model.id ?? modelKey).trim();
    if (!modelId || seen.has(modelId)) continue;
    seen.add(modelId);
    const contextWindow = model.contextWindow ?? model.limit?.context;
    members.push({
      modelId,
      ...(isPositiveInteger(contextWindow) ? { contextWindow } : {}),
    });
  }
  return members;
}

function mergeHeaders(
  ...values: Array<Record<string, string> | undefined>
): Record<string, string> | undefined {
  const result = Object.assign({}, ...values.filter(Boolean));
  return Object.keys(result).length > 0 ? result : undefined;
}

function requiresUnsupportedNoAuthentication(provider: LegacyCliProvider): boolean {
  return provider.options?.apiKeyRequired === false;
}

function isFileNotFound(error: unknown): boolean {
  return (
    typeof error === "object" &&
    error !== null &&
    "code" in error &&
    (error as { code?: unknown }).code === "ENOENT"
  );
}

function isPositiveInteger(value: unknown): value is number {
  return (
    typeof value === "number" && Number.isFinite(value) && Number.isInteger(value) && value > 0
  );
}
