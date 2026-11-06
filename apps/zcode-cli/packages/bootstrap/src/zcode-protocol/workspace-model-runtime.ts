/* eslint-disable max-lines -- workspace model protocol and compatibility request processing are still concentrated in this file. */
import { createInMemorySessionEventStore } from "@zcode/adapters/storage";
import type { ModelSelection } from "@zcode/contracts";
import {
  zcodeProviderTestModelConnectivityParamsSchema,
  zcodeWorkspaceReadPresentationParamsSchema,
  type ZCodeWorkspaceRef,
} from "@zcode/shared";
import type { ZCodeApp, ZCodeAppOptions } from "../app/types.js";
import { listProtocolSlashCommands } from "./slash-commands.js";
import {
  parseParams,
  type ZCodeProtocolAgentServerContext,
  type ZCodeProtocolSessionRecord,
} from "./server-types.js";
import { runSessionModelConfigMutation } from "../zcode-protocol-v4/model-config-mutation.js";
import { createProviderRuntimeHeadersPort } from "./provider-runtime-headers.js";

export async function readWorkspacePresentation(
  context: ZCodeProtocolAgentServerContext,
  rawParams: unknown,
) {
  const params = parseParams(zcodeWorkspaceReadPresentationParamsSchema, rawParams);
  return {
    workspace: params.workspace,
    mode: "build" as const,
    slashCommands: await listProtocolSlashCommands({
//The gray gate is the workspace level fact determined by the Host, and the directory is equipped with a read process cache.
      dynamicWorkflowEnabled: context.appRuntimePreferences.dynamicWorkflowEnabled,
      env: context.deps.env,
      logger: context.logger,
      workingDirectory: params.workspace.workspacePath,
    }),
  };
}

export async function testProviderModelConnectivity(
  context: ZCodeProtocolAgentServerContext,
  rawParams: unknown,
  abortSignal?: AbortSignal,
) {
  const params = parseParams(zcodeProviderTestModelConnectivityParamsSchema, rawParams);
// The old Personal Config cross-process watcher may permanently miss atomic write events; if the connection test is not done first
// Active refresh will repeatedly query the old Registry. The formal Registry refresh is reused here and does not bypass the creation of configuration facts.
  await context.deps.refreshProviderRegistry?.("provider-connectivity");
  const active = Array.from(context.sessions.values()).find(
    (record) => record.workspace.workspaceKey === params.workspace.workspaceKey,
  );
  const app =
    active?.app ??
    (await createWorkspaceZCodeApp(context, params.workspace, {
      env: context.deps.env,
      eventStore: createInMemorySessionEventStore(),
      runtimeConfig: { workingDirectory: params.workspace.workspacePath },
      sessionStore: context.deps.sessionStore,
      version: context.deps.version,
    }));
  try {
    await app.testModelConnectivity(
      { selection: params.selection as ModelSelection },
      { abortSignal },
    );
    return { success: true as const };
  } finally {
    if (!active) await app.close?.();
  }
}

export async function createWorkspaceZCodeApp(
  context: ZCodeProtocolAgentServerContext,
  workspace: ZCodeWorkspaceRef,
  options: Omit<ZCodeAppOptions, "providerRegistry">,
): Promise<ZCodeApp> {
  const providerRuntimeHeadersPort =
    options.providerRuntimeHeadersPort ?? createProviderRuntimeHeadersPort(context, workspace);
  return context.deps.createZCodeApp({
    ...options,
    platform: context.deps.platform,
    providerRuntimeHeadersPort,
    runtimeConfig: {
      ...options.runtimeConfig,
// createZCodeApp will normalize workingDirectory to execute cwd. Enter the protocol
// WorkspacePath is injected into the runtime separately, and session persistence can retain the path representation of the local workspaceKey.
      workspacePath: workspace.workspacePath,
// The remote session is the second layer of isolation boundary of shared-host CUA: you cannot just pass workspacePath/identity.
// Otherwise, different attachments in the same remote workspace will reuse the Accessibility frame/action state.
// Put it in this helper instead of each call point, so that both session creation paths can get the same isolation key.
      ...(workspace.remoteSessionId ? { remoteSessionId: workspace.remoteSessionId } : {}),
      ...(workspace.workspaceIdentity
        ? {
            memory: {
              ...options.runtimeConfig?.memory,
              workspaceIdentity: workspace.workspaceIdentity,
            },
          }
        : {}),
// The main session of Electron/Protocol did not explicitly enable model streaming like CLI/TUI.
// Causes the main turn to return the generateText non-streaming request. If it encounters a compatible endpoint that returns SSE, it will fail to parse according to JSON.
      modelStreaming: options.runtimeConfig?.modelStreaming ?? "on",
    },
  });
}

export function hasSessionModelProvider(
  _context: ZCodeProtocolAgentServerContext,
  record: Pick<ZCodeProtocolSessionRecord, "app" | "workspace">,
  providerId: string,
): boolean {
  return record.app.listModels().some((model) => model.ref.providerId === providerId);
}

async function ensureSessionModelAvailableUnlocked(
  context: ZCodeProtocolAgentServerContext,
  record: ZCodeProtocolSessionRecord,
): Promise<boolean> {
// When the existing model fails, it cannot be silently changed to the Registry. The first item overwrites the user's selection; the invalid selection remains unbound.
// Handled by recovery/Composer's selection checksums and initial access control; another default model fact cannot be written here.
// This transition entry is retained to allow callers of the old protocol to exit smoothly.
  void context;
  void record;
  return false;
}

export async function ensureSessionModelAvailable(
  context: ZCodeProtocolAgentServerContext,
  record: ZCodeProtocolSessionRecord,
): Promise<boolean> {
  return runSessionModelConfigMutation(record.app, () =>
    ensureSessionModelAvailableUnlocked(context, record),
  );
}

export function resolveSessionModelContextWindow(
  _context: ZCodeProtocolAgentServerContext,
  record: Pick<ZCodeProtocolSessionRecord, "app" | "workspace" | "restoredModelSelection">,
): number | undefined {
  // 缺档位会让恢复选择暂不绑定 Runtime，但模型身份仍可只读查询容量，不能伪造 20 万。
  const selection = record.app.runtime.getSessionModelSelection() ?? record.restoredModelSelection;
  const value = selection && record.app.getModelOption?.(selection)?.contextWindow;
  return typeof value === "number" && Number.isFinite(value) && value > 0 ? value : undefined;
}
