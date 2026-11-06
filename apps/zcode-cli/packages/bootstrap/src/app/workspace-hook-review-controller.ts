/* eslint-disable max-lines -- Cohesive contract/runtime module kept in one file; split is tracked separately. */
import {
  SessionEventType,
  type WorkspaceHookBundleSnapshot,
  type WorkspaceHookReasonCode,
} from "@zcode/contracts";
import {
  WorkspaceHookReviewFlowRegistry,
  createWorkspaceHookTrustRecords,
  type WorkspaceHookReviewFlow,
  type WorkspaceHookReviewTarget,
  type WorkspaceHookRuntimeAdmissionPort,
  type WorkspaceHookSnapshotEvaluation,
  type WorkspaceHookTrustCoordinator,
} from "@zcode/core";
import type {
  WorkspaceHookReviewDecision,
  WorkspaceHookReviewRequestPayload,
  WorkspaceHookTrustRevokeTarget,
} from "@zcode/shared/zcode-protocol-v4";
import { WorkspaceHookMutationError } from "@zcode/shared/workspace-hook-mutation";

export type * from "./workspace-hook-review-types.js";
import type {
  WorkspaceHookReviewCommandResult,
  WorkspaceHookReviewControllerOptions,
  WorkspaceHookReviewHostPort,
  WorkspaceHookReviewMutationPort,
  WorkspaceHookTrustStoreMutationPort,
} from "./workspace-hook-review-types.js";
import {
  buildWorkspaceHookReviewRequest,
  resolveWorkspaceHookReviewDigests,
  toWorkspaceHookReviewTarget,
} from "./workspace-hook-review-request.js";
import { WorkspaceHookReviewTelemetry } from "./workspace-hook-review-telemetry.js";
import { superviseWorkspaceHookReviewFlow } from "./workspace-hook-review-supervisor.js";
import { applyWorkspaceHookRevoke } from "./workspace-hook-review-revoke.js";

export class WorkspaceHookReviewController {
  private readonly admission: WorkspaceHookRuntimeAdmissionPort;
  private readonly appVersion?: string;
  private readonly coordinator: WorkspaceHookTrustCoordinator;
  private readonly host: WorkspaceHookReviewHostPort;
  private readonly telemetry: WorkspaceHookReviewTelemetry;
  private readonly mutation: WorkspaceHookReviewMutationPort;
  private readonly sessionId: string;
  private readonly store: Promise<WorkspaceHookTrustStoreMutationPort>;
  private readonly now: () => number;
  private readonly createId: () => string;
  private readonly registry = new WorkspaceHookReviewFlowRegistry();
/** flow → Supervise promises. WeakMap enables the flow to be automatically removed after it is recycled, without holding additional references. */
  private readonly supervisedFlows = new WeakMap<
    WorkspaceHookReviewFlow,
    Promise<void>
  >();
  private reviewFlowId?: string;
  private generation = 0;
  private mutationQueue: Promise<unknown> = Promise.resolve();

  constructor(options: WorkspaceHookReviewControllerOptions) {
    this.admission = options.admission;
    this.appVersion = options.appVersion;
    this.coordinator = options.coordinator;
    this.host = options.host;
    this.telemetry = new WorkspaceHookReviewTelemetry(
      this.admission,
      options.logger,
    );
    this.mutation = options.mutation;
    this.sessionId = options.sessionId;
    this.store = options.store;
    this.now = options.now ?? Date.now;
    this.createId = options.createId ?? (() => crypto.randomUUID());
  }

  /**
* See workspace-hook-review-supervisor: any newly opened flow must be supervised by it.
   *
* openOrReuseFlow will reuse the same flow that is still pending
* object, so repeated requestReview and revoke reopening paths may attach a supervisor to the same flow.
* Repeat emit ReviewSettled at timeout. Duplicate supervision has the same problem as lack of supervision:
* According to the flow object singleton, repeated requests directly reuse the existing supervision promise.
   */
  private superviseFlow(flow: WorkspaceHookReviewFlow): Promise<void> {
    const existing = this.supervisedFlows.get(flow);
    if (existing) return existing;
    const supervision = superviseWorkspaceHookReviewFlow({
      flow,
      host: this.host,
      registry: this.registry,
      sessionId: this.sessionId,
      telemetry: this.telemetry,
    });
    this.supervisedFlows.set(flow, supervision);
    return supervision;
  }

  /**
* Soft access control: open audit flow on demand.
   *
* Called by the requestWorkspaceHookReview command when the user clicks "Go to Review".
* When there are no pending items, it is safe no-op (returns accepted).
* Idempotent reuse (openOrReuseFlow) when there is an active flow.
* Must be supervised by superviseFlow - otherwise the flow will die silently after timeout and the panel will permanently fail.
   */
  async requestReview(target: {
    workspaceIdentity: string;
    bundleDigest: string;
  }): Promise<WorkspaceHookReviewCommandResult> {
    const snapshot = this.admission.getCurrentSnapshot();
    if (
      target.workspaceIdentity !== snapshot.workspaceIdentity ||
      target.bundleDigest !== snapshot.bundleDigest
    ) {
      return {
        accepted: false,
        reasonCode: "workspace_hooks_snapshot_mismatch" as const,
      };
    }
    const evaluation = this.coordinator.evaluateSnapshot({ snapshot });
// The old implementation only regards pending items with configuredEnabled=true as auditable items, resulting in
// Settings locks the untrusted switch to form a deadlock - disabled Hook will not run and Banner will not be triggered.
// Trusts can never be pre-established either. Configuration gate is orthogonal to Trust; review request already carries all
// snapshot items, so it can be judged by admissionClass. Disabled items will not run even after they are trusted.
    const hasPending = evaluation.items.some(
      (item) => item.admissionClass === "pending",
    );
    if (!hasPending) {
      if (evaluation.items.some((item) => item.trustState === "blocked_policy")) {
        return {
          accepted: false,
          reasonCode: "workspace_hooks_blocked_by_policy" as const,
        };
      }
      if (evaluation.storeStatus === "corrupt") {
        return {
          accepted: false,
          reasonCode: "workspace_hooks_trust_store_corrupt" as const,
        };
      }
// No pending items: safe no-op
      return { accepted: true, reviewItemIds: [] };
    }
    const flow = await this.openOrReuseFlow(snapshot, evaluation);
    void this.superviseFlow(flow).catch(() => undefined);
    return { accepted: true, reviewItemIds: [] };
  }

  respond(
    target: WorkspaceHookReviewTarget,
    decision: WorkspaceHookReviewDecision,
  ): Promise<WorkspaceHookReviewCommandResult> {
    return this.enqueueMutation(async () => {
      const validation = this.registry.validate(target, decision);
      if (!validation.accepted) {
        this.telemetry.responseRejected(target, validation.reasonCode);
        return validation;
      }
      const flow = this.registry.getCurrentFlow(this.sessionId);
      if (!flow) {
        return {
          accepted: false,
          reasonCode: "workspace_hooks_review_superseded" as const,
        };
      }
      const snapshot = this.admission.getCurrentSnapshot();
// request immutable snapshot bundle when the binding is opened. today
// replaceSnapshot has only one legal caller of toggle (via refreshPendingFlow supersede old
// flow), the equivalence check relies on this implicit invariant; once the hot listening watcher is configured, it becomes the second
// callers and bypassing refreshPendingFlow, missing tombstone will cause authorization to fall on the new bundle.
// Align explicit verification with revokeCurrent to eliminate implicit dependencies.
      if (
        target.workspaceIdentity !== snapshot.workspaceIdentity ||
        target.bundleDigest !== snapshot.bundleDigest
      ) {
        this.telemetry.responseRejected(
          target,
          "workspace_hooks_snapshot_mismatch",
        );
        return {
          accepted: false,
          reasonCode: "workspace_hooks_snapshot_mismatch" as const,
        };
      }
// Explicitly check the policy qualifications of persistent Trust before grant: within applyDecision
// If the policy rejection thrown by assertPersistentTrustMutationAllowed falls into the catch-all below,
// Will be reported as trust_store_corrupt ("trust store damaged") when the enterprise policy is tightened
// What the user sees is the error diagnosis; aligned with the revoke path: pre-check + exact reasonCode.
      if (
        !this.coordinator.canMutatePersistentTrust(snapshot.workspaceIdentity)
      ) {
        this.telemetry.responseRejected(
          target,
          "workspace_hooks_blocked_by_policy",
        );
        return {
          accepted: false,
          reasonCode: "workspace_hooks_blocked_by_policy" as const,
        };
      }
      let applied: { grantedRecordCount?: number };
      try {
        applied = await this.applyPersistentTrust(validation.reviewItemIds);
      } catch (error) {
// applyDecision can throw errors for non-storage reasons - resolveWorkspaceHookReviewDigests
// For unknown reviewItemId, coordinator internal error, store placement failure, etc. A naked catch will take all
// Failures will always be reported as trust_store_corrupt and the original errors will be completely discarded, similar to the toggle path.
// reasonCode remains unchanged (new additions require contracts review), only change
// errorMessage is transparently passed into telemetry for backtracking.
        //
// Desensitization: The message of WorkspaceHookMutationError has been desensitized upstream
// (See workspace-hook-review-mutation.ts using workspaceIdentitySummary / digestSummary).
// For any Error, only error.message is taken - the upstream error point must ensure that the message does not contain absolute path/complete digest
// (Disable reporting of complete workspace path / source path).
        this.telemetry.trustStoreFailure(
          target.bundleDigest,
          error instanceof Error ? error.message : String(error),
        );
        return {
          accepted: false,
          reasonCode: "workspace_hooks_trust_store_corrupt" as const,
        };
      }
      this.telemetry.decisionAccepted(target, decision, {
        ...(applied.grantedRecordCount === undefined
          ? {}
          : { grantedRecordCount: applied.grantedRecordCount }),
        requestEnabledCount: flow.request.items.filter(
          (item) => item.configuredEnabled,
        ).length,
      });
      const resolved = this.registry.resolve(target, decision);
// applyDecision and registry.resolve are non-atomic - if
// The deadline timer of the registry happens to be triggered, the flow becomes timed_out, and resolve returns
// superseded, so "Trust has been placed" but "Audit has expired" is reported. Users can retry and troubleshoot accordingly
// Based on this, I thought that the writing was not successful - it is also a misattribution perversion.
      //
// The decision has taken effect (the durable Trust has been placed), so press accepted to report and issue as usual
// Settled, let the front end converge to the resolved state; the rejection of resolve only means that the flow has been occupied by other final states.
// Does not mean authorization failed. The only good thing here is that it will not refer to unauthorized as authorized.
      if (!resolved.accepted) {
// Keep observation: flow has been occupied by other final states (usually deadline happens to trigger).
        this.telemetry.responseRejected(target, resolved.reasonCode);
      }
      await this.host.emit({
        type: SessionEventType.WorkspaceHookReviewSettled,
        payload: { interactionId: target.interactionId, state: "resolved" },
      });
//Soft access control: re-evaluate the pending status after settling, and notify the projection layer to update the prompt bar
      await this.emitAdmissionUpdatedAfterMutation();
//Inline item-by-item Trust cannot cause other pending items to lose their operation entry. After the old generation settles,
// If there are still pending statements, publish the next immutable generation immediately; trusted rows are set by Settings
// Disappears after refreshing, other rows continue to be operable.
      await this.refreshPendingFlow(snapshot);
      return resolved.accepted
        ? resolved
        : {
            accepted: true as const,
            reviewItemIds: [...validation.reviewItemIds],
          };
    });
  }

  toggle(
    target: WorkspaceHookReviewTarget,
    reviewItemId: string,
    enabled: boolean,
  ): Promise<
    WorkspaceHookReviewCommandResult & {
      request?: WorkspaceHookReviewRequestPayload;
    }
  > {
    return this.enqueueMutation(async () => {
      const validation = this.registry.validate(target, {
        action: "trust_selected",
        reviewItemIds: [reviewItemId],
      });
      if (!validation.accepted) return validation;
      const currentSnapshot = this.admission.getCurrentSnapshot();
      const entry = currentSnapshot.hooks.find(
        (item) => item.reviewItemId === reviewItemId,
      );
      if (!entry?.editable) {
        return {
          accepted: false,
          reasonCode: "workspace_hooks_snapshot_mismatch" as const,
        };
      }
      let writeCommitted = false;
      let nextSnapshot: WorkspaceHookBundleSnapshot;
      try {
        nextSnapshot = await this.mutation.toggle(
          { snapshot: currentSnapshot, reviewItemId, enabled },
          () => {
            writeCommitted = true;
            this.admission.invalidate("workspace_hooks_config_rebuild_failed");
          },
        );
        this.admission.replaceSnapshot(nextSnapshot);
      } catch (error) {
        if (!writeCommitted) {
// Naked catch once reported all failures of mutation port as success
// config_write_failed - including snapshot mismatch that occurs before writing to disk (after review
// bundle has changed /discovery (read failed). Based on this, the user's retry "write" always fails, which also conceals the real reason.
// Press WorkspaceHookMutationError.code for transparent transmission; telemetry adds cause for easy positioning.
          const isMutationError =
            error instanceof WorkspaceHookMutationError ||
            (error instanceof Error &&
              error.name === "WorkspaceHookMutationError");
          const mutationCode = isMutationError
            ? ((error as WorkspaceHookMutationError)
                .code as WorkspaceHookReasonCode)
            : ("workspace_hooks_config_write_failed" as const);
          this.telemetry.toggleFailure(
            target.bundleDigest,
            mutationCode,
            error instanceof Error ? error.message : String(error),
          );
          return {
            accepted: false,
            reasonCode: mutationCode as WorkspaceHookReasonCode,
          };
        }
        this.telemetry.toggleFailure(
          target.bundleDigest,
          "workspace_hooks_config_rebuild_failed",
        );
        this.registry.fail(target, "workspace_hooks_config_rebuild_failed");
        await this.host.emit({
          type: SessionEventType.WorkspaceHookReviewSettled,
          payload: {
            interactionId: target.interactionId,
            state: "configuration_error",
            reasonCode: "workspace_hooks_config_rebuild_failed",
          },
        });
        return {
          accepted: false,
          reasonCode: "workspace_hooks_config_rebuild_failed" as const,
        };
      }

      const nextFlow = await this.refreshPendingFlow(nextSnapshot);
//Soft access control:toggle re-evaluate the pending status after rebuilding the bundle
      await this.emitAdmissionUpdatedAfterMutation();
      return {
        accepted: true,
        reviewItemIds: [reviewItemId],
        ...(nextFlow ? { request: nextFlow.request } : {}),
      };
    });
  }

  revoke(
    target: WorkspaceHookReviewTarget,
    reviewItemIds: readonly string[],
  ): Promise<WorkspaceHookReviewCommandResult> {
    return this.enqueueMutation(async () => {
      const validation = this.registry.validate(target, {
        action: "trust_selected",
        reviewItemIds: [...reviewItemIds],
      });
      if (!validation.accepted) return validation;
      const snapshot = this.admission.getCurrentSnapshot();
      const result = await applyWorkspaceHookRevoke({
        coordinator: this.coordinator,
        digests: resolveWorkspaceHookReviewDigests(
          snapshot,
          validation.reviewItemIds,
        ),
        reviewItemIds: validation.reviewItemIds,
        snapshot,
        store: this.store,
      });
      if (result.accepted) {
        this.telemetry.revoked(
          snapshot.bundleDigest,
          validation.reviewItemIds.length,
        );
        await this.refreshPendingFlow(snapshot);
//Soft access control: re-evaluate the pending status after revoke
        await this.emitAdmissionUpdatedAfterMutation();
      }
      return result;
    });
  }

  revokeCurrent(
    target: WorkspaceHookTrustRevokeTarget,
  ): Promise<WorkspaceHookReviewCommandResult> {
    return this.enqueueMutation(async () => {
      const snapshot = this.admission.getCurrentSnapshot();
      if (
        target.sessionId !== this.sessionId ||
        target.remoteSessionId !== this.host.remoteSessionId ||
        target.workspaceIdentity !== snapshot.workspaceIdentity ||
        target.bundleDigest !== snapshot.bundleDigest
      ) {
        return {
          accepted: false,
          reasonCode: "workspace_hooks_snapshot_mismatch" as const,
        };
      }
      const digests = [...new Set(target.hookDeclarationDigests)];
      const entries = snapshot.hooks.filter((entry) =>
        digests.includes(entry.hookDeclarationDigest),
      );
      if (
        digests.length === 0 ||
        new Set(entries.map((entry) => entry.hookDeclarationDigest)).size !==
          digests.length
      ) {
        return {
          accepted: false,
          reasonCode: "workspace_hooks_snapshot_mismatch" as const,
        };
      }
      const result = await applyWorkspaceHookRevoke({
        coordinator: this.coordinator,
        digests,
        reviewItemIds: entries.map((entry) => entry.reviewItemId),
        snapshot,
        store: this.store,
      });
      if (result.accepted) {
        this.telemetry.revoked(snapshot.bundleDigest, digests.length);
        await this.refreshPendingFlow(snapshot);
//Soft access control: re-evaluate the pending status after revokeCurrent
        await this.emitAdmissionUpdatedAfterMutation();
      }
      return result;
    });
  }

  private async refreshPendingFlow(
    snapshot: WorkspaceHookBundleSnapshot,
  ): Promise<WorkspaceHookReviewFlow | undefined> {
    const current = this.registry.getCurrentFlow(this.sessionId);
    if (!current || current.state.state !== "pending") {
// Returning directly when there is no pending flow will make
// After "Authorization → review resolved → revocation", there will no longer be any re-authorization entry for the current session.
// Users can only create new conversations.
      //
// The semantics of revoke is revoked → admission=pending, which should be consulted again.
// Therefore, a new flow is started here when a pending item is indeed generated: the bypass of "directly granting trust" is not added.
// Authorization can still only be completed through the inline button bound to the current immutable review.
      return await this.openReviewFlowForNewPendingItems(snapshot);
    }
    const target = toWorkspaceHookReviewTarget(current.request);
    const evaluation = this.coordinator.evaluateSnapshot({ snapshot });
    const replacement = this.buildRequest(snapshot, evaluation, {
      reviewFlowId: current.request.reviewFlowId,
      generation: current.request.generation + 1,
    });
    const nextFlow = this.registry.supersede(target, replacement);
    this.telemetry.superseded(replacement);
    this.generation = replacement.generation;
    await this.host.emit({
      type: SessionEventType.WorkspaceHookReviewSuperseded,
      payload: {
        interactionId: current.request.interactionId,
        supersededByInteractionId: replacement.interactionId,
      },
    });
    this.telemetry.requestCreated(replacement);
    await this.host.emit({
      type: SessionEventType.WorkspaceHookReviewRequested,
      payload: { request: replacement },
    });
    if (replacement.summary.pendingCount === 0) {
      this.registry.closeWithoutDecision(
        toWorkspaceHookReviewTarget(replacement),
      );
      await this.host.emit({
        type: SessionEventType.WorkspaceHookReviewSettled,
        payload: {
          interactionId: replacement.interactionId,
          state: "resolved",
        },
      });
    }
    return nextFlow;
  }

  /**
* After revoke, if there is no pending flow in the current session, review will be re-enabled.
   *
* Only enabled when there are actual pending items. Switch only controls operation, trust only controls access; therefore the current review
* The configured-disabled statement in the snapshot must also preserve the inline trust entry. Start walking
* openOrReuseFlow, the same path as the first consultation, so generation / reviewFlowId /
* The existing semantics of interactionId remain unchanged.
   */
  private async openReviewFlowForNewPendingItems(
    snapshot: WorkspaceHookBundleSnapshot,
  ): Promise<WorkspaceHookReviewFlow | undefined> {
    const evaluation = this.coordinator.evaluateSnapshot({
      snapshot,
    });
    const hasPending = evaluation.items.some(
      (item) => item.admissionClass === "pending",
    );
    if (!hasPending) return undefined;
    const flow = await this.openOrReuseFlow(snapshot, evaluation);
// Must be supervised: otherwise the flow will die silently after timeout and the panel will be permanently disabled (see superviseFlow comment).
// There is deliberately no await here - the revoke command cannot be blocked by the 10-minute review deadline;
// catch is a catch-all to avoid unhandled rejections, and the flow termination itself does not generate errors that need to bubble up to the caller.
    void this.superviseFlow(flow).catch(() => undefined);
    return flow;
  }

  private async openOrReuseFlow(
    snapshot: WorkspaceHookBundleSnapshot,
    evaluation: WorkspaceHookSnapshotEvaluation,
  ): Promise<WorkspaceHookReviewFlow> {
    const current = this.registry.getCurrentFlow(this.sessionId);
    if (
      current?.state.state === "pending" &&
      current.request.bundleDigest === snapshot.bundleDigest
    ) {
      return current;
    }
    this.reviewFlowId ??= `workspace-hook-review:${this.createId()}`;
    const request = this.buildRequest(snapshot, evaluation, {
      reviewFlowId: this.reviewFlowId,
      generation: this.generation + 1,
    });
    this.generation = request.generation;
    const flow = this.registry.open(request);
    this.telemetry.requestCreated(request);
    await this.host.emit({
      type: SessionEventType.WorkspaceHookReviewRequested,
      payload: { request },
    });
    return flow;
  }

  private buildRequest(
    snapshot: WorkspaceHookBundleSnapshot,
    evaluation: WorkspaceHookSnapshotEvaluation,
    flow: { reviewFlowId: string; generation: number },
  ): WorkspaceHookReviewRequestPayload {
    return buildWorkspaceHookReviewRequest({
      snapshot,
      evaluation,
      ...flow,
      sessionId: this.sessionId,
      host: this.host,
      now: this.now,
      createId: this.createId,
    });
  }

  private async applyPersistentTrust(
    reviewItemIds: readonly string[],
  ): Promise<{ grantedRecordCount?: number }> {
    const snapshot = this.admission.getCurrentSnapshot();
    this.coordinator.assertPersistentTrustMutationAllowed(
      snapshot.workspaceIdentity,
    );
    const records = createWorkspaceHookTrustRecords({
      snapshot,
      reviewItemIds,
      grantedAt: new Date(this.now()).toISOString(),
      ...(this.appVersion ? { appVersion: this.appVersion } : {}),
    });
    const file = await (await this.store).grant(records);
    this.coordinator.replacePersistentTrustRecords(file.records, {
      status: "ok",
    });
    return { grantedRecordCount: records.length };
  }

  private enqueueMutation<T>(operation: () => Promise<T>): Promise<T> {
    const result = this.mutationQueue.then(operation, operation);
    this.mutationQueue = result.then(
      () => undefined,
      () => undefined,
    );
    return result;
  }

  /**
* Soft access control: After the mutation is completed, the pending status is re-evaluated and AdmissionUpdated is emitted.
   *
* The caliber is consistent with admission: configuredEnabled && admissionClass === "pending".
* pendingCount === 0 must also be sent, and the projection will clear the prompt bar accordingly.
* Trigger refreshEvaluation in evaluateDispatch through invalidate → of admission;
* Here, coordinator is directly used to re-evaluate the snapshot, which has the same origin as admission.emitAdmissionState.
   */
  private async emitAdmissionUpdatedAfterMutation(): Promise<void> {
    const snapshot = this.admission.getCurrentSnapshot();
    const evaluation = this.coordinator.evaluateSnapshot({ snapshot });
    const pendingCount = evaluation.items.filter(
      (item) => item.configuredEnabled && item.admissionClass === "pending",
    ).length;
    await this.host.emit({
      type: SessionEventType.WorkspaceHookAdmissionUpdated,
      payload: {
        pendingCount,
        bundleDigest: snapshot.bundleDigest,
        ...(snapshot.workspaceIdentity
          ? { workspaceIdentity: snapshot.workspaceIdentity }
          : {}),
      },
    });
  }
}
