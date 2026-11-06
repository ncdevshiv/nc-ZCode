import { randomUUID } from "node:crypto";
import { existsSync, readFileSync, renameSync, rmSync } from "node:fs";
import { access, cp, mkdir, mkdtemp, rename, rm, writeFile } from "node:fs/promises";
import { basename, dirname, join, resolve } from "node:path";
import { appendPluginSourceCleanupError, cleanupPluginSourceBestEffort } from "./helpers.js";

interface ActivateDirectoryAtomicallyInput {
  authorityPath?: string;
  prepare?: (stagedPath: string) => Promise<void>;
  signal?: AbortSignal;
  sourcePath?: string;
  targetPath: string;
}

export interface AtomicDirectoryActivation {
  finalize: () => Promise<void>;
  rollback: () => Promise<void>;
  transactionId: string;
}

interface AtomicTransactionRecordV1 {
  stageName: string;
  version: 1;
}

interface AtomicTransactionRecordV2 {
  authorityPath?: string;
  hadTarget: boolean;
  mode: "coordinated" | "standalone";
  ownerId: string;
  ownerPid: number;
  stageName: string;
  transactionId: string;
  version: 2;
}

type AtomicTransactionRecord = AtomicTransactionRecordV1 | AtomicTransactionRecordV2;

const atomicProcessOwnerId = randomUUID();
const activeAtomicReservations = new Set<string>();
const activeAtomicTargets = new Map<
  string,
  Pick<AtomicTransactionRecordV2, "authorityPath" | "hadTarget" | "mode" | "transactionId">
>();

function getAtomicRecoveryPaths(targetPath: string): {
  backupPath: string;
  transactionPath: string;
} {
  const parent = dirname(targetPath);
  const name = basename(targetPath);
  return {
    backupPath: join(parent, `.${name}.backup`),
    transactionPath: join(parent, `.${name}.transaction.json`),
  };
}

export function recoverAtomicTargetSync(targetPath: string): string {
  const parent = dirname(targetPath);
  const name = basename(targetPath);
  const { backupPath, transactionPath } = getAtomicRecoveryPaths(targetPath);
  const targetKey = resolve(targetPath);
  const activeTransaction = activeAtomicTargets.get(targetKey);

  let transaction: AtomicTransactionRecord | undefined;
  try {
    const parsed = JSON.parse(
      readFileSync(transactionPath, "utf8"),
    ) as Partial<AtomicTransactionRecord>;
    if (parsed.version === 1 && typeof parsed.stageName === "string") {
      transaction = { stageName: parsed.stageName, version: 1 };
    } else if (
      parsed.version === 2 &&
      typeof parsed.stageName === "string" &&
      typeof parsed.transactionId === "string" &&
      typeof parsed.ownerId === "string" &&
      Number.isSafeInteger(parsed.ownerPid) &&
      (parsed.ownerPid ?? 0) > 0 &&
      typeof parsed.hadTarget === "boolean" &&
      (parsed.mode === "coordinated" || parsed.mode === "standalone") &&
      (parsed.authorityPath === undefined || typeof parsed.authorityPath === "string")
    ) {
      transaction = parsed as AtomicTransactionRecordV2;
    }
  } catch {
    transaction = undefined;
  }

// The marker only indicates that the transaction has started, but cannot prove that the writer has exited. General overview/discovery
// Reading and restoring a transaction that is still active will take away the rollback backup and even delete the writer's staging.
// Return "committed generation" when reading active transactions: read backup when the authoritative status does not have transactionId, and read target after id.
  if (activeTransaction) {
    return resolveLiveAtomicReadPath(targetPath, backupPath, activeTransaction);
  }
  if (transaction?.version === 2 && isAtomicTransactionOwnerAlive(transaction)) {
    return resolveLiveAtomicReadPath(targetPath, backupPath, transaction);
  }

  if (transaction?.version === 2 && transaction.mode === "coordinated") {
    const authorityCommitted =
      typeof transaction.authorityPath === "string" &&
      authorityContainsTransactionSync(transaction.authorityPath, transaction.transactionId);
    if (authorityCommitted) {
      if (existsSync(targetPath) && existsSync(backupPath)) {
        rmSync(backupPath, { force: true, recursive: true });
      } else if (!existsSync(targetPath) && existsSync(backupPath)) {
// Theoretically, the authoritative state will only be written after the target is activated; a complete version will be restored first in case of abnormal disk status.
        renameSync(backupPath, targetPath);
      }
    } else if (existsSync(backupPath)) {
      rmSync(targetPath, { force: true, recursive: true });
      renameSync(backupPath, targetPath);
    } else if (!transaction.hadTarget) {
      rmSync(targetPath, { force: true, recursive: true });
    }
  } else if (!existsSync(targetPath) && existsSync(backupPath)) {
    renameSync(backupPath, targetPath);
  } else if (existsSync(targetPath) && existsSync(backupPath)) {
    rmSync(backupPath, { force: true, recursive: true });
  }

  if (transaction?.stageName.startsWith(`.${name}.stage-`)) {
    rmSync(join(parent, transaction.stageName), { force: true, recursive: true });
  }
  rmSync(transactionPath, { force: true });
  return targetPath;
}

function resolveLiveAtomicReadPath(
  targetPath: string,
  backupPath: string,
  transaction: Pick<
    AtomicTransactionRecordV2,
    "authorityPath" | "hadTarget" | "mode" | "transactionId"
  >,
): string {
  if (
    transaction.mode === "coordinated" &&
    transaction.authorityPath &&
    authorityContainsTransactionSync(transaction.authorityPath, transaction.transactionId)
  ) {
    return targetPath;
  }
  if (existsSync(backupPath)) return backupPath;
  if (transaction.hadTarget && existsSync(targetPath)) return targetPath;
  return backupPath;
}

function isAtomicTransactionOwnerAlive(transaction: AtomicTransactionRecordV2): boolean {
  if (transaction.ownerPid === process.pid) {
    return transaction.ownerId === atomicProcessOwnerId;
  }
  try {
    process.kill(transaction.ownerPid, 0);
    return true;
  } catch (error) {
    return (error as NodeJS.ErrnoException).code === "EPERM";
  }
}

function authorityContainsTransactionSync(authorityPath: string, transactionId: string): boolean {
  const readableAuthorityPath = recoverAtomicTargetSync(authorityPath);
  let value: unknown;
  try {
    value = JSON.parse(readFileSync(readableAuthorityPath, "utf8")) as unknown;
  } catch {
    return false;
  }
  const pending: unknown[] = [value];
  while (pending.length > 0) {
    const current = pending.pop();
    if (!current || typeof current !== "object") continue;
    if (!Array.isArray(current) && "cacheTransactionId" in current) {
      const record = current as { cacheTransactionId?: unknown };
      if (record.cacheTransactionId === transactionId) return true;
    }
    pending.push(...(Array.isArray(current) ? current : Object.values(current)));
  }
  return false;
}

async function writeAtomicTransaction(
  targetPath: string,
  stagePath: string,
  input: Pick<AtomicTransactionRecordV2, "authorityPath" | "hadTarget" | "mode" | "transactionId">,
): Promise<void> {
  const { transactionPath } = getAtomicRecoveryPaths(targetPath);
  const record: AtomicTransactionRecordV2 = {
    ...(input.authorityPath ? { authorityPath: resolve(input.authorityPath) } : {}),
    hadTarget: input.hadTarget,
    mode: input.mode,
    ownerId: atomicProcessOwnerId,
    ownerPid: process.pid,
    stageName: basename(stagePath),
    transactionId: input.transactionId,
    version: 2,
  };
  try {
// In-process reservation cannot cover multi-process writers such as CLI/Desktop.
// The marker must be created exclusively to prevent a second process from overwriting the owner and rollback snapshots of still-lived transactions.
    await writeFile(transactionPath, `${JSON.stringify(record)}\n`, {
      encoding: "utf8",
      flag: "wx",
    });
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "EEXIST") {
      throw new Error(`Atomic directory activation is already active: ${targetPath}`, {
        cause: error,
      });
    }
    throw error;
  }
}

export async function activateDirectoryAtomically(
  input: ActivateDirectoryAtomicallyInput,
): Promise<AtomicDirectoryActivation> {
  throwIfAborted(input.signal);
  const parent = dirname(input.targetPath);
  const name = basename(input.targetPath);
  await mkdir(parent, { recursive: true });
  recoverAtomicTargetSync(input.targetPath);
  const stagingContainer = await mkdtemp(join(parent, `.${name}.stage-`));
  const stagedPath = join(stagingContainer, "content");
  const { backupPath, transactionPath } = getAtomicRecoveryPaths(input.targetPath);
  const targetKey = resolve(input.targetPath);
  const transactionId = randomUUID();
  let targetMoved = false;
  let committed = false;
  let transactionWritten = false;
  let ownershipActive = false;
  let reservationActive = false;

  try {
// Fixed backup/marker can only support single writer. Allow second activation
// Overwrite the memory owner and delete the first transaction marker when cleanup fails, eventually causing rollback to lose the snapshot.
    if (activeAtomicReservations.has(targetKey) || activeAtomicTargets.has(targetKey)) {
      throw new Error(`Atomic directory activation is already active: ${input.targetPath}`);
    }
    activeAtomicReservations.add(targetKey);
    reservationActive = true;
// URL/settings marketplace only has a standardized manifest and no copyable source tree;
// It is still necessary to reuse the same set of authority generation and rollback, and cannot be returned to directly overwrite the active file.
    if (input.sourcePath) {
      await cp(input.sourcePath, stagedPath, { force: true, recursive: true });
    } else {
      await mkdir(stagedPath, { recursive: true });
    }
    await input.prepare?.(stagedPath);
    throwIfAborted(input.signal);

// The old process first rm target and then cp. Failure or cancellation of copying after downloading will delete the last available cache.
// Staging enters the submission point after complete preparations; after the submission point, the rename/status placement is completed and no longer responds to cancellation.
    const hadTarget = await pathExists(input.targetPath);
    activeAtomicTargets.set(targetKey, {
      ...(input.authorityPath ? { authorityPath: resolve(input.authorityPath) } : {}),
      hadTarget,
      mode: input.authorityPath ? "coordinated" : "standalone",
      transactionId,
    });
    ownershipActive = true;
    await writeAtomicTransaction(input.targetPath, stagingContainer, {
      ...(input.authorityPath ? { authorityPath: input.authorityPath } : {}),
      hadTarget,
      mode: input.authorityPath ? "coordinated" : "standalone",
      transactionId,
    });
    transactionWritten = true;
    if (hadTarget) {
      await rename(input.targetPath, backupPath);
      targetMoved = true;
    }
    try {
      await rename(stagedPath, input.targetPath);
      committed = true;
    } catch (error) {
      if (targetMoved && !(await pathExists(input.targetPath))) {
        try {
          await rename(backupPath, input.targetPath);
          targetMoved = false;
        } catch (rollbackError) {
          throw appendPluginSourceCleanupError(error, rollbackError);
        }
      }
      await cleanupPluginSourceBestEffort(async () => {
        await rm(transactionPath, { force: true });
      });
      transactionWritten = false;
      activeAtomicTargets.delete(targetKey);
      ownershipActive = false;
      throw error;
    }
    await cleanupPluginSourceBestEffort(async () => {
      await rm(stagingContainer, { force: true, recursive: true });
    });
    let settled = false;
    return {
      finalize: async () => {
        if (settled) return;
        settled = true;
        try {
          if (targetMoved) {
// The authoritative state with the same transactionId will be deleted only after the backup is placed.
            await cleanupPluginSourceBestEffort(async () => {
              await rm(backupPath, { force: true, recursive: true });
            });
          }
          await cleanupPluginSourceBestEffort(async () => {
            await rm(transactionPath, { force: true });
          });
        } finally {
          activeAtomicTargets.delete(targetKey);
          activeAtomicReservations.delete(targetKey);
          ownershipActive = false;
          reservationActive = false;
        }
      },
      rollback: async () => {
        if (settled) return;
        try {
// When subsequent plug-ins or installed state that rely on closure fail to write, revoke the activated directory.
          await rm(input.targetPath, { force: true, recursive: true });
          if (targetMoved) await rename(backupPath, input.targetPath);
          await cleanupPluginSourceBestEffort(async () => {
            await rm(transactionPath, { force: true });
          });
          settled = true;
        } finally {
          activeAtomicTargets.delete(targetKey);
          activeAtomicReservations.delete(targetKey);
          ownershipActive = false;
          reservationActive = false;
        }
      },
      transactionId,
    };
  } finally {
    if (!committed) {
      await cleanupPluginSourceBestEffort(async () => {
        await rm(stagingContainer, { force: true, recursive: true });
      });
      if (transactionWritten && !targetMoved) {
        await cleanupPluginSourceBestEffort(async () => {
          await rm(transactionPath, { force: true });
        });
      }
      if (ownershipActive) activeAtomicTargets.delete(targetKey);
      if (reservationActive) activeAtomicReservations.delete(targetKey);
    }
  }
}

export async function writeFileAtomically(path: string, data: string | Uint8Array): Promise<void> {
  const parent = dirname(path);
  const name = basename(path);
  await mkdir(parent, { recursive: true });
  recoverAtomicTargetSync(path);
  const temporaryPath = join(parent, `.${name}.stage-${randomUUID()}`);
  const { backupPath, transactionPath } = getAtomicRecoveryPaths(path);
  const transactionId = randomUUID();
  let targetMoved = false;
  let committed = false;
  try {
    await writeFile(temporaryPath, data);
    try {
// POSIX can directly atomically replace files in the same directory without creating a target missing window.
// If Windows refuses to overwrite existing targets, then enter the recoverable path with deterministic backup.
      await rename(temporaryPath, path);
      committed = true;
      return;
    } catch (replaceError) {
      if (!(await pathExists(path))) throw replaceError;
    }
    await writeAtomicTransaction(path, temporaryPath, {
      hadTarget: true,
      mode: "standalone",
      transactionId,
    });
    await rename(path, backupPath);
    targetMoved = true;
    try {
      await rename(temporaryPath, path);
      committed = true;
    } catch (error) {
      if (targetMoved && !(await pathExists(path))) {
        try {
          await rename(backupPath, path);
          targetMoved = false;
        } catch (rollbackError) {
          throw appendPluginSourceCleanupError(error, rollbackError);
        }
      }
      throw error;
    }
  } finally {
    await cleanupPluginSourceBestEffort(async () => {
      await rm(temporaryPath, { force: true });
    });
    if (committed && targetMoved) {
      await cleanupPluginSourceBestEffort(async () => {
        await rm(backupPath, { force: true });
      });
    }
    if (committed || !targetMoved) {
      await cleanupPluginSourceBestEffort(async () => {
        await rm(transactionPath, { force: true });
      });
    }
  }
}

async function pathExists(path: string): Promise<boolean> {
  try {
    await access(path);
    return true;
  } catch {
    return false;
  }
}

function throwIfAborted(signal: AbortSignal | undefined): void {
  if (!signal?.aborted) return;
  const error = new Error("Plugin operation cancelled");
  error.name = "AbortError";
  throw error;
}
