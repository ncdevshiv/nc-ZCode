import type { Logger, WorkspaceHookReasonCode } from "@zcode/contracts";
import {
  emitWorkspaceHookTelemetry,
  type WorkspaceHookReviewTarget,
  type WorkspaceHookRuntimeAdmissionPort,
} from "@zcode/core";
import type {
  WorkspaceHookReviewDecision,
  WorkspaceHookReviewRequestPayload,
} from "@zcode/shared/zcode-protocol-v4";

export class WorkspaceHookReviewTelemetry {
  constructor(
    private readonly admission: WorkspaceHookRuntimeAdmissionPort,
    private readonly logger?: Logger,
  ) {}

  requestCreated(request: WorkspaceHookReviewRequestPayload): void {
    this.emit("workspace_hook.review_request_created", {
      bundleDigest: request.bundleDigest,
      generation: request.generation,
// Diagnosis of the number of Trust disks: record the number of requests and actual grants, and identify silent losses.
      requestItemCount: request.items.length,
      requestEnabledCount: request.items.filter((item) => item.configuredEnabled).length,
    });
  }

  timeout(request: WorkspaceHookReviewRequestPayload): void {
    this.emit("workspace_hook.review_timeout", {
      bundleDigest: request.bundleDigest,
      generation: request.generation,
      reasonCode: "workspace_hooks_interaction_timeout",
    });
  }

  responseRejected(target: WorkspaceHookReviewTarget, reasonCode: WorkspaceHookReasonCode): void {
    this.emit(
      reasonCode === "workspace_hooks_review_superseded"
        ? "workspace_hook.stale_response"
        : "workspace_hook.snapshot_mismatch",
      { bundleDigest: target.bundleDigest, reasonCode },
    );
  }

  decisionAccepted(
    target: WorkspaceHookReviewTarget,
    decision: WorkspaceHookReviewDecision,
    counts?: { grantedRecordCount?: number; requestEnabledCount?: number },
  ): void {
    this.emit("workspace_hook.trust_selected", {
      action: decision.action,
      bundleDigest: target.bundleDigest,
      generation: target.generation,
// If it does not match requestEnabledCount, it is lost silently: decisionAccepted is only available in applyDecision
// Issued after success, so "accepted but less written" cannot be seen from the existing fields.
      ...(counts?.grantedRecordCount === undefined
        ? {}
        : { grantedRecordCount: counts.grantedRecordCount }),
      ...(counts?.requestEnabledCount === undefined
        ? {}
        : { requestEnabledCount: counts.requestEnabledCount }),
    });
  }

  /**
* Revoke observation: Whether the revocation is successful and how many items have been withdrawn need to be visible, otherwise it will be impossible to judge when troubleshooting.
   */
  revoked(bundleDigest: string, revokedCount: number): void {
    this.emit("workspace_hook.revoked", {
      bundleDigest,
      reasonCode: "workspace_hooks_revoked",
      revokedCount,
    });
  }

  trustStoreFailure(bundleDigest: string, errorMessage?: string): void {
    this.emit("workspace_hook.trust_store_failure", {
      bundleDigest,
      reasonCode: "workspace_hooks_trust_store_corrupt",
// applyDecision can throw an error due to non-storage reasons (resolveWorkspaceHookReviewDigests
// For unknown reviewItemId, coordinator internal error, etc.). If all fails, success will be reported
// trust_store_corrupt and discard cause, leaving only reasonCode in the log, making it impossible to locate the real cause for troubleshooting.
// reasonCode remains unchanged (new additions require contracts enumeration review), errorMessage is used to trace back to the real reason.
      ...(errorMessage ? { errorMessage } : {}),
    });
  }

  toggleFailure(
    bundleDigest: string,
    reasonCode: WorkspaceHookReasonCode,
    errorMessage?: string,
  ): void {
    this.emit(
// If toggle fails, press WorkspaceHookMutationError.code to transparently transmit, reasonCode overwrites write/rebuild
// Other situations: mismatch takes exclusive events, and other unclassified failures are recorded as toggle_failure.
// Avoid mistaking write-before failures as config_rebuild_failure (error attribution perversion).
      reasonCode === "workspace_hooks_snapshot_mismatch"
        ? "workspace_hook.snapshot_mismatch"
        : reasonCode === "workspace_hooks_config_rebuild_failed"
          ? "workspace_hook.config_rebuild_failure"
          : "workspace_hook.toggle_failure",
      { bundleDigest, reasonCode, ...(errorMessage ? { errorMessage } : {}) },
    );
  }

  superseded(request: WorkspaceHookReviewRequestPayload): void {
    this.emit("workspace_hook.review_superseded", {
      bundleDigest: request.bundleDigest,
      generation: request.generation,
      reasonCode: "workspace_hooks_review_superseded",
    });
  }

  private emit(
    event: Parameters<typeof emitWorkspaceHookTelemetry>[1],
    fields: Parameters<typeof emitWorkspaceHookTelemetry>[2],
  ): void {
    emitWorkspaceHookTelemetry(this.logger, event, {
      workspaceIdentity: this.admission.getCurrentSnapshot().workspaceIdentity,
      ...fields,
    });
  }
}
