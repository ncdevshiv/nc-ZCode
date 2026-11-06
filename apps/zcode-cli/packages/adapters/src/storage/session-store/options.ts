export type ForkCommitFaultStage =
  | "afterChild"
  | "afterMessages"
  | "afterGoal"
  | "afterEntries"
  | "afterInput"
  | "afterCommandFact"
  | "beforeCommit";

export interface SqliteSessionStoreOptions {
  dbPath?: string;
/** For transaction atomicity testing only; must not be set for production calls. */
  forkCommitFaultAt?: ForkCommitFaultStage;
/** For startup lock wait boundary testing only; production calls use default values. */
  startupLockTimeoutMs?: number;
}

export interface SessionStoreDebugCounts {
  sessions: number;
  messages: number;
  parts: number;
  todos: number;
  targets: number;
  sessionEntries: number;
  permissions: number;
  localSettings: number;
  schemaMigrations: number;
  inputHistory: number;
  modelUsage: number;
  toolUsage: number;
  turnUsage: number;
}
