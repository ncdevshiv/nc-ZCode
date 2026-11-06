import { createHash } from "node:crypto";
import type { Logger, WorkspaceHookReasonCode } from "@zcode/contracts";

export type WorkspaceHookTelemetryEvent =
  | "workspace_hook.feature_disabled"
  | "workspace_hook.review_request_created"
  | "workspace_hook.trust_selected"
  | "workspace_hook.review_timeout"
  | "workspace_hook.review_superseded"
  | "workspace_hook.snapshot_mismatch"
  | "workspace_hook.policy_blocked"
  | "workspace_hook.trust_store_failure"
  | "workspace_hook.toggle_failure"
  | "workspace_hook.config_rebuild_failure"
  // revoke 需要专门观测：排查「撤销后面板失效」时，必须能从日志判断
  // 撤销是否成功、并区分随后的失败属于撤销还是授权，否则定位缓慢。
  | "workspace_hook.revoked"
  | "workspace_hook.stale_response";

export interface WorkspaceHookTelemetryFields {
  workspaceIdentity?: string;
  bundleDigest?: string;
  declarationDigest?: string;
  reasonCode?: WorkspaceHookReasonCode;
  source?: string;
  action?: string;
  generation?: number;
  /**
* The errorMessage passed in from the calling side needs to be transparently transmitted: otherwise the original error will be silently discarded during emit.
* Only reasonCode is left in the log, and actual troubleshooting cannot locate the real cause.
* Only carries the short message of the domain error itself, and does not carry commands, script content or Trust payload.
   */
  errorMessage?: string;
  /**
* Diagnosis of the number of Trust disks: when the store writes less than the selected statement, decisionAccepted
* No error is thrown), the log cannot determine whether the request contains less items or the record is lost during the writing phase.
   *
* requestItemCount / requestEnabledCount records the actual number of items carried by the audit request;
* grantedRecordCount records the number of records actually written this time. If the three do not match up, silence will be lost.
   */
  requestItemCount?: number;
  requestEnabledCount?: number;
  grantedRecordCount?: number;
/** revoke The number of statements actually revoked (separate from grantedRecordCount to avoid semantic confusion). */
  revokedCount?: number;
}

const MAX_TELEMETRY_ERROR_MESSAGE_LENGTH = 300;

/**
* Workspace Hook's observations uniformly reuse the existing Logger Port.
* Only send stable reason, generation and digest, not command, script content, path or Trust payload.
 */
export function emitWorkspaceHookTelemetry(
  logger: Logger | undefined,
  event: WorkspaceHookTelemetryEvent,
  fields: WorkspaceHookTelemetryFields = {},
): void {
  if (!logger) return;
  logger.info("Workspace Hook Trust telemetry", {
    event,
    module: "workspace_hook_trust",
    ...(fields.workspaceIdentity
      ? { workspaceIdentityDigest: workspaceIdentitySummary(fields.workspaceIdentity) }
      : {}),
    ...(fields.bundleDigest ? { bundleDigest: digestSummary(fields.bundleDigest) } : {}),
    ...(fields.declarationDigest
      ? { declarationDigest: digestSummary(fields.declarationDigest) }
      : {}),
    ...(fields.reasonCode ? { reasonCode: fields.reasonCode } : {}),
    ...(fields.source ? { source: fields.source } : {}),
    ...(fields.action ? { action: fields.action } : {}),
    ...(fields.generation === undefined ? {} : { generation: fields.generation }),
    ...(fields.errorMessage
      ? { errorMessage: fields.errorMessage.slice(0, MAX_TELEMETRY_ERROR_MESSAGE_LENGTH) }
      : {}),
    ...(fields.requestItemCount === undefined ? {} : { requestItemCount: fields.requestItemCount }),
    ...(fields.requestEnabledCount === undefined
      ? {}
      : { requestEnabledCount: fields.requestEnabledCount }),
    ...(fields.grantedRecordCount === undefined
      ? {}
      : { grantedRecordCount: fields.grantedRecordCount }),
    ...(fields.revokedCount === undefined ? {} : { revokedCount: fields.revokedCount }),
  });
}

export function digestSummary(value: string): string {
  return value.length <= 12 ? value : value.slice(0, 12);
}

/**
* SHA-256 short digest of the workspace identity.
 *
* The identity of this implementation is the absolute path, and the telemetry is explicitly required not to be uploaded.
* Complete workspace path, source path is not recorded, so any text that needs to enter telemetry
* (Messages containing domain errors) must first undergo this desensitization instead of relying on downstream filtering.
 */
export function workspaceIdentitySummary(value: string): string {
  return createHash("sha256").update(value).digest("hex").slice(0, 12);
}
