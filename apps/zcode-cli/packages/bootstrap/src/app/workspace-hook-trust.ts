import { createDefaultFileWorkspaceHookTrustStore } from "@zcode/adapters/storage";
import {
  InMemoryWorkspaceHookPolicyProvider,
  WorkspaceHookTrustCoordinator,
  createWorkspaceHookRuntimeAdmission,
  emitWorkspaceHookTelemetry,
  type WorkspaceHookAdmissionState,
  type WorkspaceHookReviewTarget,
  type WorkspaceHookRuntimeAdmissionPort,
  type WorkspaceHookPolicyProvider,
} from "@zcode/core";
import { SessionEventType } from "@zcode/contracts";
import type {
  Logger,
  SessionId,
  WorkspaceHookBundleSnapshot,
  WorkspaceHookPolicy,
  WorkspaceHookAdmissionUpdatedPayload,
} from "@zcode/contracts";
import type {
  WorkspaceHookReviewDecision,
  WorkspaceHookReviewRequestPayload,
  WorkspaceHookTrustRevokeTarget,
} from "@zcode/shared/zcode-protocol-v4";
import type { WorkspaceHookRuntimeRoot } from "@zcode/shared/workspace-hook-discovery";
import {
  WorkspaceHookReviewController,
  type WorkspaceHookReviewCommandResult,
  type WorkspaceHookReviewHostPort,
  type WorkspaceHookReviewLifecycleEvent,
} from "./workspace-hook-review-controller.js";
import { createWorkspaceHookReviewMutationPort } from "./workspace-hook-review-mutation.js";
import type { WorkspaceHookReviewHostContext } from "./types.js";

interface WorkspaceHookRuntimeSecurity {
  admission: WorkspaceHookRuntimeAdmissionPort;
  snapshot: WorkspaceHookBundleSnapshot;
  /**
* Trust store is a file, and coordinator's
* persistentRecords is a per-session memory image, which is only loaded once when the session is created.
* Settings inline trust (pretrust path without task) directly writes the file and returns, running session
* The coordinator neither updates the record nor bumps the revision - the trusted Hook continues to be rejected and the banner
* pendingCount stays at the old value. After pretrust authorization is successful, this method must be called to reload the file content into this
* The coordinator of the session and resends the admission status, aligned with the respond path in the task.
   */
  reloadTrust(): Promise<void>;
/** Soft access control: open the audit flow on demand, and it is safe no-op when there are no pending items */
  requestReview(target: {
    workspaceIdentity: string;
    bundleDigest: string;
  }): Promise<WorkspaceHookReviewCommandResult>;
  respond(
    target: WorkspaceHookReviewTarget,
    decision: WorkspaceHookReviewDecision,
  ): Promise<WorkspaceHookReviewCommandResult>;
  toggle(
    target: WorkspaceHookReviewTarget,
    reviewItemId: string,
    enabled: boolean,
  ): Promise<
    WorkspaceHookReviewCommandResult & {
      request?: WorkspaceHookReviewRequestPayload;
    }
  >;
  revoke(
    target: WorkspaceHookReviewTarget,
    reviewItemIds: readonly string[],
  ): Promise<WorkspaceHookReviewCommandResult>;
  revokeCurrent(target: WorkspaceHookTrustRevokeTarget): Promise<WorkspaceHookReviewCommandResult>;
}

export function createWorkspaceHookRuntimeSecurity(input: {
  appVersion?: string;
  emitAdmissionEvent?: (event: {
    type: typeof SessionEventType.WorkspaceHookAdmissionUpdated;
    payload: WorkspaceHookAdmissionUpdatedPayload;
  }) => Promise<void>;
  emitReviewEvent?: (event: WorkspaceHookReviewLifecycleEvent) => Promise<void>;
  logger: Logger;
  projectConfigPath?: string;
  policy?: WorkspaceHookPolicy;
  policyProvider?: WorkspaceHookPolicyProvider;
  reviewHost?: WorkspaceHookReviewHostContext;
  workspaceHookTrustEnabled?: boolean;
  runtimeRoot: WorkspaceHookRuntimeRoot;
  sessionId: SessionId;
  snapshot?: WorkspaceHookBundleSnapshot;
  userConfigPath: string;
  workingDirectory: string;
/** Test injects temporary HOME; production does not transfer, Trust store falls into real ~/.zcode/security. */
  homeDir?: string;
}): WorkspaceHookRuntimeSecurity | undefined {
  if (!input.snapshot) return undefined;
  const policyProvider =
    input.policyProvider ?? new InMemoryWorkspaceHookPolicyProvider(input.policy);
  const coordinator = new WorkspaceHookTrustCoordinator({
    coordinatorEpoch: crypto.randomUUID(),
    policyProvider,
  });
// When Rollout is closed, the old hard block must be kept, and the existing Trust store cannot be read; the Trust file is retained.
// It is convenient to continue to use the choices made by the user after turning the switch back on.
  const trustEnabled = input.workspaceHookTrustEnabled === true;
  const store = trustEnabled
    ? createDefaultFileWorkspaceHookTrustStore({
        userConfigPath: input.userConfigPath,
        ...(input.homeDir ? { homeDir: input.homeDir } : {}),
      })
    : undefined;
  const ready = trustEnabled
    ? loadWorkspaceHookTrustStore({
        coordinator,
        logger: input.logger,
        store: store as NonNullable<typeof store>,
      })
    : Promise.resolve();
  let controller: WorkspaceHookReviewController | undefined;
//Soft access control:onAdmissionStateChanged → Emit WorkspaceHookAdmissionUpdated session event.
// activate() will be triggered after completing evaluate and replaceSnapshot, allowing the projection layer to write the snapshot field.
  const onAdmissionStateChanged: ((state: WorkspaceHookAdmissionState) => void) | undefined =
    input.emitAdmissionEvent
      ? (state) => {
          void input.emitAdmissionEvent!({
            type: SessionEventType.WorkspaceHookAdmissionUpdated,
            payload: {
              pendingCount: state.pendingCount,
              bundleDigest: state.bundleDigest,
              ...(state.workspaceIdentity ? { workspaceIdentity: state.workspaceIdentity } : {}),
            },
          }).catch((error: unknown) => {
            input.logger.warn("Failed to emit WorkspaceHookAdmissionUpdated", {
              errorType: error instanceof Error ? error.name : typeof error,
              event: "workspace_hook.admission_event_emit_failed",
              module: "bootstrap.workspace_hook_trust",
            });
          });
        }
      : undefined;
  const admission = createWorkspaceHookRuntimeAdmission({
    coordinator,
    enabled: trustEnabled,
    logger: input.logger,
    ready,
    ...(onAdmissionStateChanged ? { onAdmissionStateChanged } : {}),
    snapshot: input.snapshot,
  });

  if (trustEnabled && input.reviewHost && input.emitReviewEvent) {
    const host: WorkspaceHookReviewHostPort = {
      ...input.reviewHost,
      emit: input.emitReviewEvent,
    };
    controller = new WorkspaceHookReviewController({
      admission,
      appVersion: input.appVersion,
      coordinator,
      host,
      logger: input.logger,
      mutation: createWorkspaceHookReviewMutationPort({
        workingDirectory: input.workingDirectory,
        workspaceIdentity: input.snapshot.workspaceIdentity,
        projectConfigPath: input.projectConfigPath,
        runtimeRoot: input.runtimeRoot,
      }),
      sessionId: input.sessionId,
      store: store as NonNullable<typeof store>,
    });
  }

  const unavailable = (): WorkspaceHookReviewCommandResult => ({
    accepted: false,
    reasonCode: trustEnabled
      ? "workspace_hooks_require_trust_capable_host"
      : "workspace_hooks_feature_disabled",
  });
  return {
    admission,
    snapshot: input.snapshot,
    reloadTrust: async () => {
      if (!trustEnabled || !store) return;
      await loadWorkspaceHookTrustStore({ coordinator, logger: input.logger, store });
// replacePersistentTrustRecords has bumped revision and cleared evaluation cache;
// activate is idempotent (skip waiting for ready), re-evaluate and re-send the admission status
// (pendingCount === 0 will also be sent, used to clear the banner).
      await admission.activate("resume");
    },
    requestReview: (target) =>
      controller ? controller.requestReview(target) : Promise.resolve(unavailable()),
    respond: (target, decision) =>
      controller ? controller.respond(target, decision) : Promise.resolve(unavailable()),
    toggle: (target, reviewItemId, enabled) =>
      controller
        ? controller.toggle(target, reviewItemId, enabled)
        : Promise.resolve(unavailable()),
    revoke: (target, reviewItemIds) =>
      controller ? controller.revoke(target, reviewItemIds) : Promise.resolve(unavailable()),
    revokeCurrent: (target) =>
      controller ? controller.revokeCurrent(target) : Promise.resolve(unavailable()),
  };
}

async function loadWorkspaceHookTrustStore(input: {
  coordinator: WorkspaceHookTrustCoordinator;
  logger: Logger;
  store: ReturnType<typeof createDefaultFileWorkspaceHookTrustStore>;
}): Promise<void> {
  try {
    const loaded = await (await input.store).load();
    input.coordinator.replacePersistentTrustRecords(loaded.records, {
      status: loaded.status,
      ...(loaded.status === "corrupt" ? { recoveredCorruptPath: loaded.recoveredCorruptPath } : {}),
    });
  } catch (error) {
    input.coordinator.replacePersistentTrustRecords([], { status: "corrupt" });
    emitWorkspaceHookTelemetry(input.logger, "workspace_hook.trust_store_failure", {
      reasonCode: "workspace_hooks_trust_store_corrupt",
    });
    input.logger.warn("Workspace Hook Trust store bootstrap failed closed", {
      errorType: error instanceof Error ? error.name : typeof error,
      event: "workspace_hook.trust_store.bootstrap_failed",
      module: "bootstrap.workspace_hook_trust",
      reasonCode: "workspace_hooks_trust_store_corrupt",
      status: "completed",
    });
  }
}
