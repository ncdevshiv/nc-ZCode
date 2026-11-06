import { randomBytes } from "node:crypto";
import { dirname, join } from "node:path";
import { acquireFileLock } from "@zcode/shared/node";
import { ZCODE_FILE_LOCK_TIMEOUT_ERROR_CODE } from "@zcode/shared";
import type { SharedZCodeCredentialStore } from "../auth/shared-credentials.js";
import { isRecord, mcpOAuthCredentialKey } from "./oauth-credentials.js";

const MCP_OAUTH_PENDING_AUTHORIZATION_KEY = "pending_authorization";

/**
* Competition for authorized leases pending budget.
 *
* Cannot be used 0: `acquireFileLock` will first determine `elapsed >= maxWaitMs` during competition, and then try to recycle
* abandoned lock. Zero wait will time out directly and never execute owner-dead check; authorization function after the lock-holding process crashes
* will be permanently unavailable. Giving a short budget can ensure "at least one owner-dead recycling before failure", and at the same time, follower
* The additional delay is limited to a magnitude that is insignificant in the human-machine authorization process.
 */
const AUTHORIZATION_LEASE_MAX_WAIT_MS = 250;
const AUTHORIZATION_LEASE_RETRY_DELAYS_MS = [25] as const;
const AUTHORIZATION_LEASE_OWNERLESS_GRACE_MS = 100;

interface McpOAuthAuthorizationLease {
  attemptId: string;
  release(): Promise<void>;
}

/**
* lease file path.
 *
* basename only allows hash and hyphen: credential key prefix is ​​in the form of `mcp:oauth:<hash>`, with colon in
* The Windows file name is illegal. Directly entering the path will cause the entire authorization process to fail on Windows.
 */
function resolveAuthorizationLeasePath(credentialsFilePath: string, keyPrefix: string): string {
  return join(dirname(credentialsFilePath), `${sanitizeKeyPrefix(keyPrefix)}.authz`);
}

export function sanitizeKeyPrefix(keyPrefix: string): string {
  return keyPrefix.replaceAll(/[^a-zA-Z0-9-]/g, "-");
}

/**
* Try to become an authorized leader. If it cannot be grabbed, it returns `undefined` (the caller switches to follower), and does not block and wait.
 *
* Competition with the same process is also guaranteed by the mutual exclusion of `mkdir`: the second caller's `mkdir` receives EEXIST, and then reads
* The owner PID is the process itself and is alive, so it will not be recycled by mistake and will be downgraded to a follower correctly. So no additional
* In-process registry.
 */
export async function tryAcquireAuthorizationLease(input: {
  credentialsFilePath: string;
  keyPrefix: string;
}): Promise<McpOAuthAuthorizationLease | undefined> {
  const leasePath = resolveAuthorizationLeasePath(input.credentialsFilePath, input.keyPrefix);
  try {
    const release = await acquireFileLock(
      leasePath,
      AUTHORIZATION_LEASE_RETRY_DELAYS_MS,
      AUTHORIZATION_LEASE_OWNERLESS_GRACE_MS,
      AUTHORIZATION_LEASE_MAX_WAIT_MS,
    );
    return {
      attemptId: randomBytes(16).toString("hex"),
      release,
    };
  } catch (error) {
    if (getErrorCode(error) === ZCODE_FILE_LOCK_TIMEOUT_ERROR_CODE) return undefined;
// EACCES/EPERM, etc. indicate that the credential directory is not writable, interactive authorization is unlikely to succeed in any case, and must be reported instead of silently downgraded.
    throw error;
  }
}

interface PendingAuthorizationRecord {
  attemptId: string;
  authorizationUrl: string;
  baselineGeneration?: string;
  expiresAt: number;
  state: string;
}

interface StoredPendingAuthorization {
  attempt_id: string;
  authorization_url: string;
  baseline_generation?: string;
  expires_at: number;
  state: string;
}

export async function publishPendingAuthorization(
  credentialStore: SharedZCodeCredentialStore,
  keyPrefix: string,
  record: PendingAuthorizationRecord,
): Promise<void> {
  const stored: StoredPendingAuthorization = {
    attempt_id: record.attemptId,
    authorization_url: record.authorizationUrl,
    ...(record.baselineGeneration === undefined
      ? {}
      : { baseline_generation: record.baselineGeneration }),
    expires_at: record.expiresAt,
    state: record.state,
  };
  await credentialStore.save(pendingKey(keyPrefix), JSON.stringify(stored));
}

/** Read pending; expired data is considered non-existent (TTL is only used to display judgment and does not assume lock ownership semantics). */
export async function loadPendingAuthorization(
  credentialStore: SharedZCodeCredentialStore,
  keyPrefix: string,
  now = Date.now(),
): Promise<PendingAuthorizationRecord | undefined> {
  const raw = await credentialStore.load(pendingKey(keyPrefix));
  if (!raw) return undefined;
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return undefined;
  }
  if (
    !isRecord(parsed) ||
    typeof parsed.attempt_id !== "string" ||
    typeof parsed.authorization_url !== "string" ||
    typeof parsed.state !== "string" ||
    typeof parsed.expires_at !== "number"
  ) {
    return undefined;
  }
  if (parsed.expires_at <= now) return undefined;
  return {
    attemptId: parsed.attempt_id,
    authorizationUrl: parsed.authorization_url,
    ...(typeof parsed.baseline_generation === "string"
      ? { baselineGeneration: parsed.baseline_generation }
      : {}),
    expiresAt: parsed.expires_at,
    state: parsed.state,
  };
}

/**
* Only delete the pending published by this attempt.
 *
* Deletion must be done by attempt CAS: if the `finally` of the old leader is unconditionally deleted, it will erase the `finally` of the new leader.
* pending, the follower will lose the authorization URL. First read the current value to confirm ownership, and then compare-and-delete according to the original value.
 */
export async function deletePendingAuthorizationIfOwned(
  credentialStore: SharedZCodeCredentialStore,
  keyPrefix: string,
  attemptId: string,
): Promise<boolean> {
  const key = pendingKey(keyPrefix);
  const raw = await credentialStore.load(key);
  if (!raw) return false;
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
// Unresolvable residual values ​​are only cleaned up if they are indeed the current value.
    return await credentialStore.deleteIfValue(key, raw);
  }
  if (!isRecord(parsed) || parsed.attempt_id !== attemptId) return false;
  return await credentialStore.deleteIfValue(key, raw);
}

function pendingKey(keyPrefix: string): string {
  return mcpOAuthCredentialKey(keyPrefix, MCP_OAUTH_PENDING_AUTHORIZATION_KEY);
}

function getErrorCode(error: unknown): string | undefined {
  if (typeof error !== "object" || error === null || !("code" in error)) return undefined;
  const code = (error as { code?: unknown }).code;
  return typeof code === "string" ? code : undefined;
}
