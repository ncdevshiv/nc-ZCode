import {
  classifyDatabaseStartupError,
  databaseMigrationIdSchema,
  databaseStartupErrorDetails,
  type DatabaseMigrationFacts,
} from "@zcode/shared";
import { createHash } from "node:crypto";
import type { DatabaseSync } from "node:sqlite";
import { SqliteSessionMigrationError } from "./errors.js";
import { SQLITE_MIGRATIONS } from "./migrations.js";
import type { SchemaMigrationRow } from "./rows.js";

export const DEFAULT_SQLITE_STARTUP_LOCK_TIMEOUT_MS = 5_000;

const SQLITE_BUSY = 5;
const WAL_RETRY_INITIAL_DELAY_MS = 10;
const WAL_RETRY_MAX_DELAY_MS = 200;
const waitBuffer = new Int32Array(new SharedArrayBuffer(Int32Array.BYTES_PER_ELEMENT));

export type SqliteMigrationPhase =
  | "checking"
  | "waiting_for_lock"
  | "migrating"
  | "committing"
  | "ready"
  | "failed";

export interface SqliteMigrationProgress {
  phase: SqliteMigrationPhase;
  migration?: DatabaseMigrationFacts;
  elapsedMs: number;
  migrationId?: string;
  completed?: number;
  total?: number;
  errorCode?: string;
  sqliteCode?: number;
  systemCode?: string;
}

export interface AsyncSqliteMigrationOptions {
  lockWaitTimeoutMs?: number;
  onProgress?: (progress: SqliteMigrationProgress) => Promise<void>;
}

const DEFAULT_SQLITE_MIGRATION_WAIT_MS = 60 * 60_000;
const ASYNC_NATIVE_BUSY_TIMEOUT_MS = 25;
type MigrationStep = SqliteMigrationProgress | { delayMs: number };

export function runSqliteSessionMigrations(
  db: DatabaseSync,
  dbPath: string,
  lockTimeoutMs = DEFAULT_SQLITE_STARTUP_LOCK_TIMEOUT_MS,
): void {
  for (const step of migrationSteps(db, dbPath, lockTimeoutMs)) {
    if ("delayMs" in step) waitSync(step.delayMs);
  }
}

export async function runSqliteSessionMigrationsAsync(
  db: DatabaseSync,
  dbPath: string,
  options: AsyncSqliteMigrationOptions = {},
): Promise<void> {
// Synchronous SQLite only waits for a short time; the real startup wait gives way to the event loop, and the notification can be flushed first.
//Business connections cannot inherit the one-hour waiting budget for upgrade. After the migration is completed (including failure), the original 5-second policy will be restored.
  db.exec(`pragma busy_timeout = ${ASYNC_NATIVE_BUSY_TIMEOUT_MS}`);
  const steps = migrationSteps(
    db,
    dbPath,
    options.lockWaitTimeoutMs ?? DEFAULT_SQLITE_MIGRATION_WAIT_MS,
  );
  let failed = false;
  try {
    for (const step of steps) {
      if ("delayMs" in step)
        await new Promise<void>((resolve) => setTimeout(resolve, step.delayMs));
      else if (step.phase === "failed") {
        try {
          await options.onProgress?.(step);
        } catch {
/* Error notification failure cannot overwrite the original database exception. */
        }
      } else await options.onProgress?.(step);
    }
  } catch (error) {
    failed = true;
    throw error;
  } finally {
// Notification transmission failure will also close the generator's transaction, and the semi-migrated connection cannot be handed over to the business.
    try {
      steps.return();
      db.exec(`pragma busy_timeout = ${DEFAULT_SQLITE_STARTUP_LOCK_TIMEOUT_MS}`);
    } catch (error) {
      if (!failed) throw error;
    }
  }
}

function* migrationSteps(
  db: DatabaseSync,
  dbPath: string,
  lockTimeoutMs: number,
): Generator<MigrationStep, void> {
  const startedAt = Date.now();
  const deadline = startedAt + Math.max(0, lockTimeoutMs);
  let migrationFacts: DatabaseMigrationFacts | undefined;
  const progress = (phase: SqliteMigrationPhase): SqliteMigrationProgress => ({
    phase,
    elapsedMs: Date.now() - startedAt,
    ...(migrationFacts ? { migration: { ...migrationFacts } } : {}),
  });
  let transactionStarted = false;
  function* acquire(operation: () => void): Generator<MigrationStep, void> {
//Each lock-taking phase is reported independently: the unknown wait before preflight cannot swallow up the wait after confirmed migration.
    let waitingReported = false;
    let retryDelayMs = WAL_RETRY_INITIAL_DELAY_MS;
    while (true) {
      try {
        operation();
        return;
      } catch (error) {
// SQLITE_LOCKED often refers to same-connection/shared-cache conflicts; only retry SQLITE_BUSY for external writer competition.
        if (!isSqliteBusyError(error)) throw error;
        const remainingMs = deadline - Date.now();
        if (remainingMs <= 0) throw lockTimeoutError(error, dbPath);
        if (!waitingReported) {
          waitingReported = true;
          yield progress("waiting_for_lock");
        }
        yield { delayMs: Math.min(remainingMs, retryDelayMs) };
        retryDelayMs = Math.min(WAL_RETRY_MAX_DELAY_MS, retryDelayMs * 2);
      }
    }
  }
  try {
    yield progress("checking");
    db.exec("pragma foreign_keys = on");
    yield* acquire(() => {
      const current = readJournalMode(db);
      if (current === "wal" || isMemoryJournalMode(dbPath, current)) return;
      const mode = readJournalMode(db, true);
      if (mode !== "wal" && !isMemoryJournalMode(dbPath, mode)) {
        throw new SqliteSessionMigrationError(
          `SQLite refused WAL journal mode for ${dbPath}; received ${mode}`,
          { dbPath, kind: "sql_failed" },
        );
      }
    });
// The preflight only determines the display; the actual execution is still checked one by one after the lock is obtained, and other windows will not be migrated again after completion.
    yield* acquire(() => {
      migrationFacts = {
        kind: inspectMigrationKind(db, dbPath),
        executedCount: 0,
        committedCount: 0,
      };
    });
    yield progress("checking");
// SQLite is the only coordinator; after taking the lock, the real ledger is read, and the waiter will not repeatedly migrate submitted items.
    yield* acquire(() => db.exec("begin immediate"));
    transactionStarted = true;
    db.exec(`create table if not exists schema_migration (
      id text primary key, checksum text not null, app_version text, time_applied integer not null
    )`);
// Record the starting point after taking the lock to avoid that other writers have not yet submitted during preflight; subsequent migrations must not overwrite it.
    const baseline = db.prepare("SELECT id FROM schema_migration ORDER BY id DESC LIMIT 1").get();
    if (migrationFacts)
      migrationFacts.lastAppliedMigrationId = baseline
        ? databaseMigrationIdSchema.safeParse(baseline.id).data
        : null;
    let completed = 0;
    for (const migration of SQLITE_MIGRATIONS) {
      try {
        const checksum = migrationChecksum(migration.sql);
        const applied = readAppliedMigration(db, migration.id);
        if (applied) {
          ensureMigrationChecksum(migration.id, applied.checksum, checksum, dbPath);
          completed++;
          continue;
        }
// When a new item to be executed appears after preflight, the type will be promoted based on the ledger in the lock. SQL cannot be executed with none.
        if (migrationFacts?.kind === "none") migrationFacts.kind = "upgrade";
        yield {
          ...progress("migrating"),
          migrationId: migration.id,
          completed,
          total: SQLITE_MIGRATIONS.length,
        };
// This hotfix only adjusts the startup wait; execution and ledger verification share the frozen original SQL to avoid maintaining two sets of conversion semantics.
        db.exec(migration.sql);
        if (migrationFacts) migrationFacts.executedCount++;
        db.prepare(
          "insert into schema_migration (id, checksum, app_version, time_applied) values (?, ?, ?, ?)",
        ).run(migration.id, checksum, migration.appVersion, Date.now());
        completed++;
      } catch (error) {
        throw normalizeMigrationError(error, dbPath, migration.id);
      }
    }
    yield progress("committing");
    db.exec("commit");
    transactionStarted = false;
    if (migrationFacts) migrationFacts.committedCount = migrationFacts.executedCount;
    yield progress("ready");
  } catch (error) {
// Roll back first, then notify the failure; the notification cannot allow the business to see the failed transaction that is still within the transaction.
    if (transactionStarted && db.isTransaction) {
      try {
        db.exec("rollback");
      } catch {
/* Recover when closing the connection, retaining the original exception. */
      }
    }
    transactionStarted = false;
    const normalized = normalizeMigrationError(error, dbPath);
    yield {
      ...progress("failed"),
      errorCode: normalized.kind,
      ...databaseStartupErrorDetails(normalized),
      migrationId: normalized.migrationId,
    };
    throw normalized;
  } finally {
    if (transactionStarted && db.isTransaction) {
      try {
        db.exec("rollback");
      } catch {
/* The caller closes the connection and retains the original exception. */
      }
    }
  }
}

function isSqliteBusyError(error: unknown): boolean {
  return (
    typeof error === "object" &&
    error !== null &&
    "errcode" in error &&
    typeof error.errcode === "number" &&
    (error.errcode & 0xff) === SQLITE_BUSY
  );
}

function isMemoryJournalMode(dbPath: string, journalMode: string): boolean {
  return dbPath === ":memory:" && journalMode === "memory";
}

function readJournalMode(db: DatabaseSync, enableWal = false): string {
  const row = db.prepare(enableWal ? "pragma journal_mode = wal" : "pragma journal_mode").get() as
    | Record<string, unknown>
    | undefined;
  const mode = row?.journal_mode;
  return typeof mode === "string" ? mode.toLowerCase() : "unknown";
}

function waitSync(delayMs: number): void {
  Atomics.wait(waitBuffer, 0, 0, Math.max(1, Math.ceil(delayMs)));
}

function normalizeMigrationError(
  error: unknown,
  dbPath: string,
  migrationId?: string,
): SqliteSessionMigrationError {
  if (error instanceof SqliteSessionMigrationError) return error;
  if (isSqliteBusyError(error)) return lockTimeoutError(error, dbPath, migrationId);
  return new SqliteSessionMigrationError(
    migrationId
      ? `SQLite migration ${migrationId} failed for ${dbPath}`
      : `SQLite migration initialization failed for ${dbPath}`,
    {
      cause: error,
      dbPath,
      kind: classifyDatabaseStartupError(error),
      migrationId,
    },
  );
}

function lockTimeoutError(
  cause: unknown,
  dbPath: string,
  migrationId?: string,
): SqliteSessionMigrationError {
  return new SqliteSessionMigrationError(
    `Timed out waiting for SQLite migration lock at ${dbPath}`,
    { cause, dbPath, kind: "lock_timeout", migrationId },
  );
}

function readAppliedMigration(db: DatabaseSync, id: string): SchemaMigrationRow | undefined {
  return db.prepare("select id, checksum from schema_migration where id = ?").get(id) as
    | SchemaMigrationRow
    | undefined;
}

function migrationChecksum(sql: string): string {
  return createHash("sha256").update(sql.trim()).digest("hex");
}

function ensureMigrationChecksum(
  id: string,
  applied: string,
  current: string,
  dbPath: string,
): void {
  if (applied === current) return;
  throw new SqliteSessionMigrationError(
    `SQLite migration checksum mismatch for ${id}. Historical migrations are immutable; add a new migration instead.`,
    {
      dbPath,
      kind: "checksum_mismatch",
      migrationId: id,
    },
  );
}

/** Read only the small migration ledger, and do not scan the message table; the history database with a table but no ledger is an upgrade. */
function inspectMigrationKind(db: DatabaseSync, dbPath: string): DatabaseMigrationFacts["kind"] {
  const hasLedger = db
    .prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name='schema_migration'")
    .get();
  let pending = false;
  for (const migration of SQLITE_MIGRATIONS) {
    const row = hasLedger ? readAppliedMigration(db, migration.id) : undefined;
    if (row)
      ensureMigrationChecksum(migration.id, row.checksum, migrationChecksum(migration.sql), dbPath);
    else pending = true;
  }
  if (!pending) return "none";
  const hasDataTables = db
    .prepare(
      "SELECT 1 FROM sqlite_master WHERE type='table' AND name NOT IN ('schema_migration', 'sqlite_sequence') LIMIT 1",
    )
    .get();
  return hasDataTables ? "upgrade" : "initialize";
}
