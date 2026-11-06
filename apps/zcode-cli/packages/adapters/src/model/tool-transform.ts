// ============================================================
// Vercel AI SDK tool transforms
// ============================================================

import { anthropic } from "@ai-sdk/anthropic";
import { jsonSchema, tool, type ToolSet } from "ai";
import { ModelErrorCode, type JsonSchema, type ModelToolContract } from "@zcode/contracts";
import { AiSdkModelAdapterError } from "./errors.js";
import { isAnthropicFirstPartyModelId, toStrictToolSchema } from "./strict-tool-schema.js";

export interface AiSdkToolTransformOptions {
  requiresMfjsToolSchema?: boolean;
  supportsNativeWebSearch?: boolean;
  providerKind?: "openai" | "anthropic" | "openai-compatible" | "gateway" | "custom";

/** The model id of this request is only used for first-party strict qualification determination; if it is absent, strict will not be enabled. */
  modelId?: string;
}

export function toAiSdkTools(
  tools?: ModelToolContract[],
  options: AiSdkToolTransformOptions = {},
): ToolSet | undefined {
  if (!tools || tools.length === 0) {
    return undefined;
  }

  const entries = tools.flatMap((contract): [string, ToolSet[string]][] => {
    if (contract.providerNative) {
      const providerTool = toAiSdkProviderNativeTool(contract, options);
      return providerTool ? [[contract.name, providerTool]] : [];
    }

    const strictSchema = resolveStrictToolSchema(contract, options);
    const baseTool = {
      description: contract.description,
      inputSchema: jsonSchema<unknown>(
        normalizeToolInputSchema(
          contract.name,
          strictSchema ?? (contract.inputSchema as JsonSchema),
          options,
        ),
      ),
      ...(strictSchema === undefined ? {} : { strict: true }),
      needsApproval: contract.needsApproval,
      ...providerOptionsForClientTool(options),
    };

    if (!contract.execute) {
      return [[contract.name, tool<unknown, never>(baseTool)]];
    }

    return [
      [
        contract.name,
        tool<unknown, unknown>({
          ...baseTool,
          execute: async (input, options) =>
            contract.execute?.(input, {
              toolCallId: options.toolCallId,
              abortSignal: options.abortSignal,
              metadata: {
                readOnly: contract.readOnly,
                destructive: contract.destructive,
                concurrentSafe: contract.concurrentSafe,
                requiresUserInteraction: contract.requiresUserInteraction,
                sideEffectScope: contract.sideEffectScope,
                maxOutputBytes: contract.maxOutputBytes,
                timeoutMs: contract.timeoutMs,
                experimentalContext: options.experimental_context,
              },
            }),
        }),
      ],
    ];
  });

  return entries.length > 0 ? (Object.fromEntries(entries) as ToolSet) : undefined;
}

/**
* When the contract declares strict, the provider capabilities match, the model is qualified as the first party, and the schema is expressible, strict schema is returned;
* If any condition is not met, undefined is returned, and the caller continues to send the original schema to prevent the compatible end from receiving unfamiliar fields or illegal shapes.
 */
function resolveStrictToolSchema(
  contract: ModelToolContract,
  options: AiSdkToolTransformOptions,
): JsonSchema | undefined {
  if (contract.strict !== true) return undefined;
  if (options.providerKind !== "anthropic") return undefined;
  if (!isAnthropicFirstPartyModelId(options.modelId)) return undefined;
  return toStrictToolSchema(contract.inputSchema as JsonSchema);
}

function normalizeToolInputSchema(
  toolName: string,
  schema: JsonSchema,
  options: AiSdkToolTransformOptions,
): JsonSchema {
  if (!options.requiresMfjsToolSchema) {
    return schema;
  }
  return hoistLocalJsonSchemaRefsToDefs(toolName, schema);
}

function hoistLocalJsonSchemaRefsToDefs(toolName: string, schema: JsonSchema): JsonSchema {
  const refs = new Set<string>();
  collectNonDefsLocalRefs(schema, refs);
  const unresolvedRef = [...refs].find((ref) => resolveLocalJsonPointer(schema, ref) === undefined);
  if (unresolvedRef) {
    throw new AiSdkModelAdapterError(
      ModelErrorCode.InvalidModelRequest,
      `Tool ${toolName} contains an unresolvable local schema reference ${unresolvedRef}`,
      { context: { toolName, ref: unresolvedRef } },
    );
  }
  if (refs.size === 0) {
    return schema;
  }

  const existingDefs = asPlainRecord(schema.$defs) ?? {};
  const usedDefKeys = new Set(Object.keys(existingDefs));
  const defKeyByRef = new Map<string, string>();
  let nextDefIndex = 0;
  for (const ref of refs) {
    let defKey = `zcode_ref_${nextDefIndex}`;
    while (usedDefKeys.has(defKey)) {
      nextDefIndex += 1;
      defKey = `zcode_ref_${nextDefIndex}`;
    }
    nextDefIndex += 1;
    usedDefKeys.add(defKey);
    defKeyByRef.set(ref, defKey);
  }

  const rewrittenRoot = rewriteJsonSchemaRefs(schema, defKeyByRef) as JsonSchema;
  const rewrittenDefs = asPlainRecord(rewrittenRoot.$defs) ?? {};
  const hoistedDefs: Record<string, unknown> = {};
  for (const [ref, defKey] of defKeyByRef) {
    hoistedDefs[defKey] = rewriteJsonSchemaRefs(resolveLocalJsonPointer(schema, ref), defKeyByRef);
  }

// MCP schema will reuse #/properties/..., but K3 only accepts #/$defs/....
// Copy the reference target at the model adaptation boundary and rewrite the reference. This will neither modify the original tool contract nor affect other models.
  return {
    ...rewrittenRoot,
    $defs: {
      ...rewrittenDefs,
      ...hoistedDefs,
    },
  };
}

function collectNonDefsLocalRefs(value: unknown, refs: Set<string>): void {
  if (Array.isArray(value)) {
    for (const item of value) {
      collectNonDefsLocalRefs(item, refs);
    }
    return;
  }
  const record = asPlainRecord(value);
  if (!record) return;
  if (
    typeof record.$ref === "string" &&
    record.$ref.startsWith("#/") &&
    !record.$ref.startsWith("#/$defs/")
  ) {
    refs.add(record.$ref);
  }
  for (const child of Object.values(record)) {
    collectNonDefsLocalRefs(child, refs);
  }
}

function resolveLocalJsonPointer(root: unknown, ref: string): unknown {
  if (!ref.startsWith("#/")) return undefined;
  let current = root;
  try {
    for (const encodedSegment of ref.slice(2).split("/")) {
      const segment = decodeURIComponent(encodedSegment)
        .replaceAll("~1", "/")
        .replaceAll("~0", "~");
      if (Array.isArray(current)) {
        if (!/^\d+$/u.test(segment)) return undefined;
        current = current[Number(segment)];
        continue;
      }
      const record = asPlainRecord(current);
      if (!record || !Object.prototype.hasOwnProperty.call(record, segment)) {
        return undefined;
      }
      current = record[segment];
    }
  } catch {
    return undefined;
  }
  return current;
}

function rewriteJsonSchemaRefs(value: unknown, defKeyByRef: ReadonlyMap<string, string>): unknown {
  if (Array.isArray(value)) {
    return value.map((item) => rewriteJsonSchemaRefs(item, defKeyByRef));
  }
  const record = asPlainRecord(value);
  if (!record) return value;
  return Object.fromEntries(
    Object.entries(record).map(([key, child]) => {
      if (key === "$ref" && typeof child === "string") {
        const defKey = defKeyByRef.get(child);
        if (defKey) {
          return [key, `#/$defs/${defKey}`];
        }
      }
      return [key, rewriteJsonSchemaRefs(child, defKeyByRef)];
    }),
  );
}

function asPlainRecord(value: unknown): Record<string, unknown> | undefined {
  return value && typeof value === "object" && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : undefined;
}

function providerOptionsForClientTool(options: AiSdkToolTransformOptions): {
  providerOptions?: Record<string, Record<string, boolean>>;
} {
  if (options.providerKind !== "anthropic") {
    return {};
  }

  // AI SDK enables Anthropic fine-grained tool input streaming by default
  // and serializes eager_input_streaming on every function tool. Several
  // Anthropic-compatible gateways reject that extra tool field, so ZCode opts out
  // at the provider-option boundary unless a future capability contract enables it.
  return {
    providerOptions: {
      anthropic: {
        eagerInputStreaming: false,
      },
    },
  };
}

function toAiSdkProviderNativeTool(
  contract: ModelToolContract,
  options: AiSdkToolTransformOptions,
): ToolSet[string] | undefined {
  if (contract.providerNative?.logicalName !== "WebSearch") {
    return undefined;
  }

  const args = contract.providerNative.args ?? {};
  const maxUses = numberArg(args.maxUses) ?? 8;
  const allowedDomains = stringArrayArg(args.allowedDomains);
  const blockedDomains = stringArrayArg(args.blockedDomains);

  switch (options.providerKind) {
    case "anthropic":
      if (!options.supportsNativeWebSearch) {
        throw new AiSdkModelAdapterError(
          ModelErrorCode.InvalidModelRequest,
          "Effective Model Config does not support provider-native WebSearch",
        );
      }
      return anthropic.tools.webSearch_20260209({
        maxUses,
        allowedDomains,
        blockedDomains,
      }) as ToolSet[string];

    default:
      throw new AiSdkModelAdapterError(
        ModelErrorCode.InvalidModelRequest,
        `Provider API kind ${options.providerKind ?? "unknown"} does not encode provider-native WebSearch`,
      );
  }
}

function numberArg(value: unknown): number | undefined {
  return typeof value === "number" && Number.isFinite(value) ? value : undefined;
}

function stringArrayArg(value: unknown): string[] | undefined {
  if (!Array.isArray(value)) return undefined;
  const values = value.filter((item): item is string => typeof item === "string");
  return values.length > 0 ? values : undefined;
}
