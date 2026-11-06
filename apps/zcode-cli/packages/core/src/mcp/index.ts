/* eslint-disable max-lines -- Cohesive contract/runtime module kept in one file; split is tracked separately. */
// MCP tool bridge - projects MCP descriptors into core tool entries

import {
  modelMessageContentToText,
  ZCODE_MCP_ERROR_PRESENTATION_MESSAGE_ONLY,
  ZCODE_MCP_ERROR_PRESENTATION_META_KEY,
  type JsonSchema,
  type McpPort,
  type McpToolCallResult,
  type McpToolDescriptor,
  type ModelMessageContent,
  type ModelMessageContentBlock,
  type ModelToolSideEffectScope,
  type PermissionCapabilityGroup,
  type RiskLevel,
} from "@zcode/contracts";
import { ZCODE_CUA_OFFICIAL_MCP_NAMESPACE_NAME as ZCODE_CUA_OFFICIAL_MCP_SERVER_NAME } from "@zcode/shared";
import { OFFICIAL_CUA_FRAME_MODEL_CONTENT_PROTECTION } from "@zcode/zcode-cua/frame-contract";
import type { ToolRegistry } from "../tool/registry.js";
import type { ToolEntry } from "../tool/types.js";
import { createToolRuleNameSet } from "../tool/tool-visibility.js";
import {
  asDataUrl,
  base64PayloadFromMcpImageData,
  normalizeMcpToolResultForModel,
} from "./image-normalization.js";
import { toMcpToolName, toModelVisibleMcpNamePart } from "./name.js";

export { toMcpToolName } from "./name.js";

export {
  HOST_NODE_REPL_IMAGE_MAX_DIMENSION,
  MCP_IMAGE_INLINE_BASE64_BYTES,
  MCP_IMAGE_INLINE_RAW_BYTES,
} from "./image-normalization.js";

const MCP_TOOL_TIMEOUT_MS = 30_000;
const OFFICIAL_CUA_PERMISSION_CAPABILITY_GROUP = "official_cua" satisfies PermissionCapabilityGroup;
const CUA_USER_TITLE_SCHEMA = {
  type: "string",
  minLength: 1,
  maxLength: 120,
  description:
    "Required short user-facing title in the user's language that describes why the app interface is being read without implementation terms such as CUA, MCP, or get_app_state",
} satisfies JsonSchema;
const ZCODE_CUA_CANONICAL_MODEL_PREFIX = "mcp__computer-use__";
const ZCODE_CUA_PROVIDER_SPELLING_ALIAS_PREFIX = "mcp__computer_use__";

export interface RegisterMcpToolsOptions {
  allowedTools?: readonly string[];
  disallowedTools?: readonly string[];
  /**
* Official CUA server authenticated by the runtime using unforgeable product authority credentials.
* The name itself does not constitute trust; when omitted, it is fail-closed and all MCPs are treated as normal tools.
* Does not project official CUA canonical names, nor mount provider spell aliases.
   */
  officialCuaServerNames?: ReadonlySet<string>;
}

export function registerMcpTools(
  registry: ToolRegistry,
  mcpPort: McpPort,
  descriptors: readonly McpToolDescriptor[],
  options: RegisterMcpToolsOptions = {},
): string[] {
  const allowed = options.allowedTools ? new Set(options.allowedTools) : undefined;
  const disallowed = createToolRuleNameSet(options.disallowedTools);
  const registered: string[] = [];

  for (const descriptor of descriptors) {
    const officialCuaAuthorityVerified =
      options.officialCuaServerNames?.has(descriptor.serverName) === true;
    const descriptorName = toMcpToolName(descriptor);
    const name = toRegisteredMcpToolName(descriptor, officialCuaAuthorityVerified);
// After the official CUA projects the main name of the model, if the rules are only checked according to the new name, the namespaced saved before the upgrade
// denylist will be silently invalidated and allowed. If either the old or new name hits deny, it will be rejected, and if any one of the old and new names hits allow, it will be accepted.
    if (allowed && !allowed.has(name) && !allowed.has(descriptorName)) continue;
    if (disallowed?.has(name) || disallowed?.has(descriptorName)) continue;
    registry.register(createMcpToolEntry(name, descriptor, mcpPort, officialCuaAuthorityVerified));
    registered.push(name);
  }

  return registered;
}

function toRegisteredMcpToolName(
  descriptor: McpToolDescriptor,
  officialCuaAuthorityVerified: boolean,
): string {
  if (
    officialCuaAuthorityVerified &&
    descriptor.serverName === ZCODE_CUA_OFFICIAL_MCP_SERVER_NAME
  ) {
// The adapter will namespace the official plug-in serverName, so descriptor.name is
// mcp__plugin_zcode-cua_computer-use__*; using it directly will make the computer-use agreed by the provider
// Tools never exist. After the trust gate is established, only the visible name of the projection model is used, and the handler still uses the original route of the descriptor.
    return `${ZCODE_CUA_CANONICAL_MODEL_PREFIX}${toModelVisibleMcpNamePart(descriptor.toolName)}`;
  }
  return toMcpToolName(descriptor);
}

function createMcpToolEntry(
  name: string,
  descriptor: McpToolDescriptor,
  mcpPort: McpPort,
  officialCuaAuthorityVerified: boolean,
): ToolEntry {
  const readOnly = descriptor.annotations?.readOnlyHint === true;
  const destructive = descriptor.annotations?.destructiveHint === true;
  const isHostNodeReplExecution =
    descriptor.serverName === "node_repl" && descriptor.toolName === "js";
  const isCuaAppObservation = isZCodeCuaGetAppState(descriptor);
// The js hosting node_repl can execute native Node code and cannot use the medium/network of ordinary unknown MCP
// Default value; otherwise the permissions UI will incorrectly describe file/process level capabilities as normal network calls.
  const sideEffectScope: ModelToolSideEffectScope = isHostNodeReplExecution ? "system" : "network";
  const riskLevel: RiskLevel = isHostNodeReplExecution
    ? "high"
    : destructive
      ? "high"
      : readOnly
        ? "low"
        : "medium";
  const needsApproval = true;
  const timeoutMs = descriptor.timeoutMs ?? MCP_TOOL_TIMEOUT_MS;
  const resultBudget = officialCuaAuthorityVerified
    ? {
// The base64 of the image block is not included in the model text budget, but the tree text may still exceed that of ordinary MCP
// 50 KiB. This gives the official CUA enough bounded text space to avoid universal truncation and structural
// image/image_ref degenerates into a pure string or changes the adjacent order.
        maxInlineBytes: 256 * 1024,
        maxModelBytes: 256 * 1024,
        strategy: "truncate" as const,
        preview: { direction: "head" as const },
      }
    : isHostNodeReplExecution
      ? {
          maxInlineBytes: 1_000_000,
          maxModelBytes: 64 * 1024,
          strategy: "artifact" as const,
          preview: { direction: "tail" as const, maxBytes: 64 * 1024 },
          artifact: { enabled: true, retention: "session" as const },
        }
      : {
          maxInlineBytes: 100_000,
          maxModelBytes: 50_000,
          strategy: "truncate" as const,
          preview: { direction: "head" as const },
        };

  return {

// Tool not found is returned directly due to precise search. Only established and internal to an unforgeable official authority
// When serverName is still the official namespaced name, it is a one-way alias; the provider continues to only look at the canonical name.
    aliases: officialCuaProviderSpellingAliases(name, descriptor, officialCuaAuthorityVerified),
    capability: `MCP tool exposed by ${descriptor.serverName}: ${descriptor.toolName}`,
// Project-level CUA authorization can only reuse the unforgeable official authority gate.
// The server/tool ​​name can be counterfeited by third parties, so the name wildcard must not be used to express this permission.
    ...(officialCuaAuthorityVerified
      ? {
          permissionCapabilityGroup: OFFICIAL_CUA_PERMISSION_CAPABILITY_GROUP,
// The final raster and the following image_ref together define the only pixel coordinate system available to the model.
// modelContentProtection is the only Host authority; general resultBudget / hook
// The projection accordingly cannot truncate, discard or rearrange this chunk, avoiding parallel boolean drift.
          modelContentProtection: OFFICIAL_CUA_FRAME_MODEL_CONTENT_PROTECTION,
        }
      : {}),
    inputSchema: createModelFacingMcpInputSchema(descriptor, isCuaAppObservation),
    outputSchema: McpToolOutputJsonSchema,
    metadata: {
      concurrentSafe: readOnly || descriptor.annotations?.idempotentHint === true,
      destructive,
// The description of the MCP tool must be transparently transmitted to the metadata, so that the registry can bring it into the model input.
// Otherwise, only name + inputSchema will be seen on the model side, and there will be no basis for judgment when calling the MCP tool.
      description: descriptor.description,
      name,
      mcpPresentation: {
        serverName: descriptor.serverName,
        toolName: descriptor.toolName,
        ...(descriptor.description ? { description: descriptor.description } : {}),
// Only official MCP results are allowed to carry structured identifiers trusted by the client (limit exhausted/no package).
        ...(descriptor.official ? { official: true } : {}),
      },
      needsApproval,
      readOnly,
      riskLevel,
      sideEffectScope,
      timeoutMs,
    },
    permission: {
      permission: "mcp",
      reason: `MCP tool ${descriptor.serverName}/${descriptor.toolName} executes through an external server`,
      riskLevel,
      sideEffectScope,
      needsApproval,
      patternSources: ["toolName", "input", "network"],
      denyPriority: "beforeAsk",
    },
    resultBudget,
    timeout: {
      defaultMs: timeoutMs,
      allowCallOverride: false,
    },
    cancellation: {
      supported: true,
      cleanup: "bestEffort",
      userVisibleMessage: `MCP tool ${name} was cancelled`,
    },
    trace: {
      required: true,
      propagateToAdapters: true,
      recordInput: "summary",
      recordOutput: "summary",
    },
    handler: async (input, context) => {
      const result = await mcpPort.callTool(
        {
          serverName: descriptor.serverName,
          toolName: descriptor.toolName,
          arguments: toMcpRuntimeArguments(input, isCuaAppObservation),
          trace: {
            traceId: context.traceId,
            spanId: context.spanId,
            parentSpanId: context.parentSpanId,
            sessionId: context.sessionId,
            turnId: context.turnId,
          },
          runtimeScope: context.runtimeScope ?? "main",
          workspacePath: context.workingDirectory,
          ...(context.remoteSessionId ? { remoteSessionId: context.remoteSessionId } : {}),
          ...(context.workspaceIdentity?.trim()
            ? {
                workspaceIdentity: context.workspaceIdentity.trim(),
                workspaceKey: context.workspaceIdentity.trim(),
              }
            : { workspaceKey: context.workingDirectory }),
          ...(context.turnId ? { turnId: context.turnId } : {}),
          clientMode: context.clientMode ?? "desktop-continuous",
          deliveryKind: context.deliveryKind ?? "desktop-continuous",
        },
        {
          signal: context.abortSignal,
          timeoutMs,
        },
      );
// MCP server will return a large base64 image; resultBudget only sees the image placeholder text.
// A copy must be saved in the handler stage and the visible content of the model must be replaced to prevent the provider request body from being exploded.
      return normalizeMcpToolResultForModel({
        compressOversizedImages: isHostNodeReplExecution,
        context,
        descriptor,
        preserveOfficialCuaFrames: officialCuaAuthorityVerified,
        result,
        toolName: name,
      });
    },
    formatModelContent: (output) => formatMcpToolResult(output),
  };
}

function officialCuaProviderSpellingAliases(
  name: string,
  descriptor: McpToolDescriptor,
  officialCuaAuthorityVerified: boolean,
): readonly string[] | undefined {
  if (
    !officialCuaAuthorityVerified ||
    descriptor.serverName !== ZCODE_CUA_OFFICIAL_MCP_SERVER_NAME ||
    !name.startsWith(ZCODE_CUA_CANONICAL_MODEL_PREFIX)
  ) {
    return undefined;
  }
  const toolName = name.slice(ZCODE_CUA_CANONICAL_MODEL_PREFIX.length);
  return toolName.length > 0
    ? [`${ZCODE_CUA_PROVIDER_SPELLING_ALIAS_PREFIX}${toolName}`]
    : undefined;
}

export const McpToolOutputJsonSchema = {
  type: "object",
  required: ["content"],
  properties: {
    content: {
      type: "array",
      items: {
        type: "object",
        additionalProperties: true,
      },
    },
    structuredContent: {},
    isError: {
      type: "boolean",
    },
    _meta: {
      type: "object",
      additionalProperties: true,
    },
  },
  additionalProperties: false,
} satisfies JsonSchema;

function normalizeInputSchema(schema: JsonSchema | undefined): JsonSchema {
  if (!schema || typeof schema !== "object") {
    return {
      type: "object",
      properties: {},
      additionalProperties: true,
    };
  }

  return {
    ...schema,
    type: "object",
    properties:
      schema.properties &&
      typeof schema.properties === "object" &&
      !Array.isArray(schema.properties)
        ? schema.properties
        : {},
  };
}

function createModelFacingMcpInputSchema(
  descriptor: McpToolDescriptor,
  isCuaAppObservation: boolean,
): JsonSchema {
  const schema = normalizeInputSchema(descriptor.inputSchema);
  if (!isCuaAppObservation) return schema;

  const properties = schema.properties as Record<string, unknown>;
  const required = Array.isArray(schema.required)
    ? schema.required.filter((value): value is string => typeof value === "string")
    : [];

// Reason: title is a summary of the intention shown to the user by ZCode and does not belong to the upstream zcode-cua parameters. Only in model contract
// Overlay the required fields, and then strip them off with runtime dispatch, which not only allows the model to stably generate readable titles, but also maintains strict schema compatibility with the upstream.
  return {
    ...schema,
    properties: {
      ...properties,
      title: CUA_USER_TITLE_SCHEMA,
    },
    required: [...new Set([...required, "title"])],
  };
}

function isZCodeCuaGetAppState(
  descriptor: Pick<McpToolDescriptor, "serverName" | "toolName">,
): boolean {
  if (descriptor.toolName.trim().toLowerCase().replace(/-/g, "_") !== "get_app_state") {
    return false;
  }

  const serverName = descriptor.serverName.trim().toLowerCase().replace(/_/g, "-");
  return (
    descriptor.serverName === ZCODE_CUA_OFFICIAL_MCP_SERVER_NAME ||
    serverName === "zcode-cua" ||
    serverName === "computer-use" ||
    (serverName.includes("zcode-cua") && serverName.includes("computer-use"))
  );
}

function hasInformativeStructuredContent(value: unknown): boolean {
  if (value === null || value === undefined) return false;
  if (Array.isArray(value)) return value.length > 0;
  if (typeof value === "object") return Object.keys(value).length > 0;
  return true;
}

function formatMcpToolResult(output: unknown): ModelMessageContent {
  if (!isMcpToolCallResult(output)) {
    return stringify(output);
  }

  const blocks = output.content.flatMap(formatContentBlock);
// The production adapter will retain the empty keys of structuredContent; undefined, null, empty objects and
// Empty arrays have no model information, and fake "Structured content" blocks cannot be appended. Contentful error details
// Still needs to be retained, permission guidance relies on this structured channel.
  if (hasInformativeStructuredContent(output.structuredContent)) {
    blocks.push({
      type: "text",
      text: `Structured content:\n${stringify(output.structuredContent)}`,
    });
  }

  const content = blocks.length > 0 ? collapseModelBlocks(blocks) : stringify(output);
  if (!output.isError) return content;
// The display strategy is explicitly declared by MCP result; the general bridge should not identify the specific server.
// Nor should you parse the error string to guess what belongs on the stack.
  const errorPresentation = output._meta?.[ZCODE_MCP_ERROR_PRESENTATION_META_KEY];
  return typeof errorPresentation === "string" &&
    errorPresentation === ZCODE_MCP_ERROR_PRESENTATION_MESSAGE_ONLY
    ? content
    : `MCP tool returned an error:\n${modelMessageContentToText(content)}`;
}

function formatContentBlock(block: Record<string, unknown>): ModelMessageContentBlock[] {
  if (block.type === "text" && typeof block.text === "string") {
    return block.text.length > 0 ? [{ type: "text", text: block.text }] : [];
  }
  if (block.type === "image") {
    const mimeType = typeof block.mimeType === "string" ? block.mimeType : "unknown";
    if (typeof block.data === "string" && typeof block.mimeType === "string") {
      return [
        {
          type: "image",
          mediaType: block.mimeType,
          dataUrl: asDataUrl(block.data, block.mimeType),
          source: {
            id: "mcp-image",
            kind: "inline",
            mimeType: block.mimeType,
            placeholder: "MCP image",
            sizeBytes: estimateBase64Bytes(block.data),
          },
        },
      ];
    }
    return [{ type: "text", text: `[MCP image content omitted: ${mimeType}]` }];
  }
  if (block.type === "audio") {
    const mimeType = typeof block.mimeType === "string" ? block.mimeType : "unknown";
    return [{ type: "text", text: `[MCP audio content omitted: ${mimeType}]` }];
  }
  if (block.type === "resource") {
    return [{ type: "text", text: `MCP resource content:\n${stringify(block.resource ?? block)}` }];
  }
  return [{ type: "text", text: stringify(block) }];
}

function collapseModelBlocks(blocks: ModelMessageContentBlock[]): ModelMessageContent {
  if (blocks.every((block) => block.type === "text")) {
    return blocks.map((block) => (block.type === "text" ? block.text : "")).join("\n\n");
  }
  return blocks;
}

function estimateBase64Bytes(value: string): number | undefined {
  const data = base64PayloadFromMcpImageData(value);
  if (data.length === 0) return undefined;
  return Math.floor((data.replace(/=+$/, "").length * 3) / 4);
}

function isMcpToolCallResult(value: unknown): value is McpToolCallResult {
  return (
    typeof value === "object" &&
    value !== null &&
    Array.isArray((value as McpToolCallResult).content)
  );
}

function toRecordInput(input: unknown): Record<string, unknown> {
  if (input && typeof input === "object" && !Array.isArray(input)) {
    return input as Record<string, unknown>;
  }
  return {};
}

function toMcpRuntimeArguments(
  input: unknown,
  stripCuaUserTitle: boolean,
): Record<string, unknown> {
  const argumentsRecord = toRecordInput(input);
  if (!stripCuaUserTitle || !("title" in argumentsRecord)) return argumentsRecord;

  const runtimeArguments = { ...argumentsRecord };
  delete runtimeArguments.title;
  return runtimeArguments;
}

function stringify(value: unknown): string {
  try {
    return JSON.stringify(value, null, 2) ?? "";
  } catch {
    return String(value);
  }
}
