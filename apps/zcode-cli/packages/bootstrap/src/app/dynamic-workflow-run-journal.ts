// ============================================================
// Dynamic Workflow Run Service: journal capability detection and store narrowing
// ============================================================
// dynamic-workflow-run-service.ts reaches the upper limit of oxlint max-lines (400 lines), and replaces journal /
// The structural narrowing of the task link store and the four sets of capability detection are split into this file; the public side is still from dynamic-workflow-run-service.ts
// Export.
//
// Every detection here follows the same discipline: store is a port and the implementation is replaceable, so it is detected based on capabilities rather than instanceof;
// Absence means **visible downgrade** (corresponding to the reading surface not being implemented/returning to empty/not being constructed), and will never be silently returned to the memory implementation.

import type { DwfRunIntrospectionQueries, DwfRunSessionListItem } from "@zcode/adapters/storage";
import type { CreateSessionTaskLinkInput, Logger, SessionStorePort } from "@zcode/contracts";
import type { JournalStorePort, RunRecord } from "@zcode/dynamic-workflow";

/** Task link is dropped into the library (production is SqliteSessionStore; testing is passed to spy). */
export interface DynamicWorkflowTaskLinkStore {
  createSessionTaskLink(input: CreateSessionTaskLinkInput): Promise<unknown>;
}

/**
* task link Structural narrowing of the storage area. Same processing as {@link resolveDynamicWorkflowJournalStore}:
* store is a port, and the implementation is replaceable, so it is detected by capabilities instead of instanceof. Skip building link when missing——
* The actor session itself is still in the database, but there is one less belonging edge in the session tree (can be downgraded, without affecting the correctness of the run).
 */
export function isDynamicWorkflowTaskLinkStore(
  store: SessionStorePort | undefined,
): store is SessionStorePort & DynamicWorkflowTaskLinkStore {
  return (
    typeof (store as { createSessionTaskLink?: unknown } | undefined)?.createSessionTaskLink ===
    "function"
  );
}

/**
* Narrow out the dwf journal from the session store. Structural judgment rather than instanceof - store is a port,
* Implement replaceable (precedent: isScriptWorkflowStore, script-workflow-utils.ts).
 */
export function resolveDynamicWorkflowJournalStore(
  sessionStore: SessionStorePort | undefined,
  logger?: Logger,
): JournalStorePort | undefined {
  const candidate = sessionStore as
    | (SessionStorePort & { workflowJournalStore?: () => JournalStorePort })
    | undefined;
  if (typeof candidate?.workflowJournalStore !== "function") {
// Visible downgrade: Port not constructed → CreateWorkflow returns to placeholder diagnostics. Never return the memory journal——
// That will make run appear to be running, but silently throw everything away when the process exits.
    logger?.info?.("Dynamic workflow run service disabled: session store has no dwf journal", {
      event: "dynamic_workflow.run_service.unavailable",
      module: "bootstrap.app",
      reason: "session_store_missing_workflow_journal_store",
    });
    return undefined;
  }
  return candidate.workflowJournalStore();
}

/**
* Host side journal side: engine port + narrow query for orphan convergence and enumeration.
 *
* Deliberately ** not widening ** the engine's {@link JournalStorePort}: the engine only reads and writes its own row according to runId, never according to the parent session
* Find run - these two queries are the requirements of the host. Adding the domain port is equivalent to requiring each journal implementation to do things that the engine does not do.
* responsible (the memory implementation does not provide them). Same processing as {@link isDynamicWorkflowTaskLinkStore}: capability detection
* Instead of instanceof, store is a port, implementation replaceable.
 */
interface DynamicWorkflowJournalStore extends JournalStorePort {
  listNonTerminalRuns(parentSessionId: string): RunRecord[];
  /**
* Runs under the name of a certain parent session, most recently updated first, up to limit items (enumeration surface, service listRunsForSession).
   *
* Return {@link DwfRunSessionListItem} instead of `RunRecord`: `RunRecord` is deliberately not timed (the engine does not
* care), and the enumeration side should report `updatedAt`. Narrow projection does not read `result_json` (unbounded product, list is not displayed),
* But **keep failure** - the predicate of `resumable` depends on failure.code.
   */
  listRunsByParentSession(parentSessionId: string, limit: number): DwfRunSessionListItem[];
}

/** Whether the journal has narrow queries required for orphan convergence (the production SQLite implementation does, the engine's memory implementation does not). */
export function supportsNonTerminalRunQuery(
  journal: JournalStorePort,
): journal is JournalStorePort & Pick<DynamicWorkflowJournalStore, "listNonTerminalRuns"> {
  return (
    typeof (journal as Partial<DynamicWorkflowJournalStore>).listNonTerminalRuns === "function"
  );
}

/** Whether journal has enumeration narrow query. Two queries are detected independently: if one is missing, only the corresponding reading surface will be downgraded, not consecutively. */
export function supportsRunEnumeration(
  journal: JournalStorePort,
): journal is JournalStorePort & Pick<DynamicWorkflowJournalStore, "listRunsByParentSession"> {
  return (
    typeof (journal as Partial<DynamicWorkflowJournalStore>).listRunsByParentSession === "function"
  );
}

/**
* Journal with run introspection query (the access base of `ListWorkflowRuns` / `GetWorkflowRun`).
 *
* The **only source** of signatures is adapters' {@link DwfRunIntrospectionQueries} (`import type`, runtime
*Zero dependencies). I deliberately don’t copy it by hand here: these four queries are not on the engine’s {@link JournalStorePort} (the engine never
* enumeration run, nor aggregate counting), so they can only be connected by capability detection - once the signature drifts, the compiler will say nothing,
* Only two tools will be silently downgraded to "This session does not have this capability".
 */
export interface DynamicWorkflowIntrospectableJournal
  extends JournalStorePort, DwfRunIntrospectionQueries {}

/**
* Whether journal has run introspection query. **Four items together**: Ability is holistic (the list requires listRuns, and the details require another three items),
* Partially present implementations will simply cause a tool to blow up at runtime, rather than visibly degrading it.
 */
export function supportsRunIntrospection(
  journal: JournalStorePort,
): journal is DynamicWorkflowIntrospectableJournal {
  const candidate = journal as Partial<DwfRunIntrospectionQueries>;
  return (
    typeof candidate.countNodesByStatus === "function" &&
    typeof candidate.getRunRow === "function" &&
    typeof candidate.listRecentLogEvents === "function" &&
    typeof candidate.listRuns === "function"
  );
}
