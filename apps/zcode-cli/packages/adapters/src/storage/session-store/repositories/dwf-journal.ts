/**
* SQLite implementation of `JournalStorePort` (dwf_* tables).
 *
* The port is synchronous, repository-like, and fits nicely into node:sqlite's DatabaseSync - so the engine doesn't have to be persistent
* Introducing asynchronous gaps, the certainty of replay is not affected by storage implementation.
 *
* Dependency direction with @zcode/dynamic-workflow: `import type` only. The port belongs to the domain package and this file is its
* An adapter; no value must be taken from the domain package at runtime (`implements` is erased at compile time).
 *
* Transactionality is not on the port side: Scenarios that require the same atoms as session writes are combined by the driver using `begin immediate`.
 */

import type { DatabaseSync } from "node:sqlite";
import type {
  ActorRecord,
  JournalStorePort,
  ListEventsOptions,
  NodeRecord,
  RunEvent,
  RunRecord,
  RunSettlementRecord,
  RunStatus,
  StoredEvent,
} from "@zcode/dynamic-workflow";
import { encodeJson } from "../json.js";
// The product reading interface is a module of its own: it only has one db handle and has no shared state with the write-read of run/actor/node/event.
// And its two queries each carry a large argument of "why this number source, this sorting, and this cursor".
import {
  listArtifactItems,
  listArtifactRows,
  type DwfArtifactItem,
  type DwfArtifactItemsQuery,
} from "./dwf-journal-artifacts.js";
import {
  decodeActor,
  decodeEvent,
  decodeNode,
  decodeRun,
  encodeResultJson,
  encodeRunSettlement,
  type DwfActorRow,
  type DwfEventRow,
  type DwfNodeRow,
  type DwfWorldNodeRow,
  type DwfRunDetailRow,
  type DwfRunListItem,
  type DwfRunRow,
  type DwfRunSessionListItem,
} from "./dwf-journal-codecs.js";
import {
  countNodesByStatus,
  getRunRow,
  listRecentLogEvents,
  listRuns,
  listRunsByParentSession,
  listWorldNodes,
  type DwfListRunsQuery,
  type DwfNodeStatusCounts,
  type DwfRunIntrospectionQueries,
} from "./dwf-journal-introspection.js";

export type { DwfArtifactItem, DwfArtifactItemsQuery } from "./dwf-journal-artifacts.js";
// run introspection reading surface (DwfRunIntrospectionQueries and its SQL) lives in dwf-journal-introspection.ts
//(max-lines split); the type is still exported from this file, and the existing importer does not need to change the path.
export type {
  DwfListRunsQuery,
  DwfNodeStatusCounts,
  DwfRunIntrospectionQueries,
} from "./dwf-journal-introspection.js";

class SqliteDwfJournalStore implements JournalStorePort, DwfRunIntrospectionQueries {
  constructor(private readonly db: DatabaseSync) {}

  createRun(record: RunRecord): void {
    const now = Date.now();
// Logical → Physical (zero migration final state encoding, see dwf-journal-codecs.ts file header).
    const settlement = encodeRunSettlement(record.status, record);
    try {
      this.db
        .prepare(
          `
          insert into dwf_run (
            id, parent_session_id, cwd, name, script_text, script_hash, tool_call_id,
            args_json, resumed_from, caps_max_concurrency,
            spent_tokens, status, failure_json, result_json, time_created, time_updated
          ) values (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
          `,
        )
        .run(
          record.runId,
          record.parentSessionId ?? null,
          record.cwd ?? null,
// name is the same metadata path as scriptText / cwd: it is only written at the moment of build run, and there will be no other metadata path after that.
// The writer touches it (updateRunStatus intentionally does not list this column).
          record.name ?? null,
          record.scriptText ?? null,
          record.scriptHash ?? null,
          record.toolCallId ?? null,
// Actually participate in the same metadata path of scriptText: it is only written at the moment of build run, and there will be no writers thereafter.
// Touch it - resume The one read back and replayed must be the approved one.
          encodeJson(record.args),
// lineage belongs to this write-once metadata path: revision is supersede, predecessor line is zero-touched,
// So "Who revised this run from" can only be written at the moment when the run is created.
          record.resumedFrom ?? null,
          record.caps.maxConcurrency,
          record.spentTokens,
          settlement.status,
          settlement.failureJson,
          encodeResultJson(record.result),
          now,
          now,
        );
    } catch (error) {
// Repeated run is a contract error of the caller, which deserves a message that can be directly read; other failures (FK, disk, constraints) are thrown up as they are.
      if (this.getRun(record.runId) !== undefined) {
        throw new Error(`dwf journal: run already exists: ${record.runId}`, { cause: error });
      }
      throw error;
    }
  }

  getRun(runId: string): RunRecord | undefined {
    const row = this.db.prepare("select * from dwf_run where id = ?").get(runId) as
      | DwfRunRow
      | undefined;
    return row ? decodeRun(row) : undefined;
  }

  updateRunStatus(runId: string, status: RunStatus, settlement?: RunSettlementRecord): void {
    const now = Date.now();
// Non-final state = None settlement: resume When turning run back to running, the failure_json of the previous life must be cleared /
// result_json——The resume branch of the engine does not have a settlement bag, and the final state semantics of "absent key = not touched" will make the orphan converge
//The written Interrupted fails and runs concurrently. Conflicting settlement bag (not final but carrying failure/result)
// Same as clearing (contract test is nailed, the two implementations have the same semantics).
    if (status === "pending" || status === "running") {
      const { changes } = this.db
        .prepare(
          `
          update dwf_run set
            status = ?,
            failure_json = null,
            result_json = null,
            time_updated = ?
          where id = ?
          `,
        )
        .run(status, now, runId);
      this.assertRunTouched(changes, runId);
      return;
    }
//Final state and product **written in one stroke**: two UPDATEs will open a crash window, creating "completed but result_json
// is empty" - it is the loss category that the 0020 column should be closed.
    //
// `failure_json` **Whole column rewriting** (without coalesce). Orphans of the second desktop instance
// Convergence writes failed + Interrupted on the run where the engine of this process is still alive; the engine then completes normally, and
// coalesce leaves the external failure intact - the line says "done" and "interrupted" at the same time. The settlement bag is that moment
// Failed **Full Truth**: stopped writes the envelope forever, errored writes its own failure (absent is NULL, that's the truth),
// completed writes NULL.
    //
// `result_json` reserved coalesce: The semantics of the product are "absent = not touched" (contract use case "keeps an
// already-settled artifact when a later write omits it"), a repeated settlement without artifacts should not erase it.
    const encoded = encodeRunSettlement(status, settlement);
    const { changes } = this.db
      .prepare(
        `
        update dwf_run set
          status = ?,
          failure_json = ?,
          result_json = coalesce(?, result_json),
          time_updated = ?
        where id = ?
        `,
      )
      .run(encoded.status, encoded.failureJson, encodeResultJson(settlement?.result), now, runId);
    this.assertRunTouched(changes, runId);
  }

  updateRunUsage(runId: string, spentTokens: number): void {
    const { changes } = this.db
      .prepare("update dwf_run set spent_tokens = ?, time_updated = ? where id = ?")
      .run(spentTokens, Date.now(), runId);
    this.assertRunTouched(changes, runId);
  }

  /**
* All **non-final** runs under the name of a parent session. Intentionally not on `JournalStorePort`: engine never looks by parent session
* run, this query only serves orphan convergence on the host side - a run where a process is killed will always stop at `running`,
* Converge it by the next app construction of the same session (`bootstrap/src/app/dynamic-workflow-run-service.ts`,
* Call this method according to ability detection).
   *
* This method only produces SQL: the final state set here is **index-friendly pre-screening**, and the determination authority remains on the service side (after it gets the record
*Go through it again according to your own final state set). `parent_session_id` must be entered into the condition - cleaning without it will be the same process brother
* Session ** is flying** run marked dead (same sqlite, separate memory registries). SQL's `= ?` naturally does not match
* `NULL`, so rows whose parent session is absent do not belong to any session and will not be touched by any convergence.
   */
  listNonTerminalRuns(parentSessionId: string): RunRecord[] {
//Physical final state set (failed/cancelled is the drop-out state of logical errored/stopped, and will not be migrated).
    const rows = this.db
      .prepare(
        `
        select * from dwf_run
        where parent_session_id = ?
          and status not in ('completed', 'failed', 'cancelled')
        order by id
        `,
      )
      .all(parentSessionId) as unknown as DwfRunRow[];
    return rows.map(decodeRun);
  }

// ---- Host side run introspection reading surface: SQL and argument live in dwf-journal-introspection.ts, only delegation is done here.

  listRuns(query: DwfListRunsQuery): DwfRunListItem[] {
    return listRuns(this.db, query);
  }

  getRunRow(runId: string): DwfRunDetailRow | undefined {
    return getRunRow(this.db, runId);
  }

  countNodesByStatus(runId: string): DwfNodeStatusCounts {
    return countNodesByStatus(this.db, runId);
  }

  listRecentLogEvents(runId: string, limit: number): StoredEvent[] {
    return listRecentLogEvents(this.db, runId, limit);
  }

  listRunsByParentSession(parentSessionId: string, limit: number): DwfRunSessionListItem[] {
    return listRunsByParentSession(this.db, parentSessionId, limit);
  }


  putActor(record: ActorRecord): void {
    const now = Date.now();
    this.db
      .prepare(
        `
        insert into dwf_actor (
          run_id, site_id, ordinal, name, persona_json, session_id, resolved_model,
          time_created, time_updated
        ) values (?, ?, ?, ?, ?, ?, ?, ?, ?)
        on conflict(run_id, site_id, ordinal) do update set
          name = excluded.name,
          persona_json = excluded.persona_json,
          session_id = excluded.session_id,
          resolved_model = excluded.resolved_model,
          time_updated = excluded.time_updated
        `,
      )
      .run(
        record.runId,
        record.siteId,
        record.ordinal,
        record.name ?? null,
        encodeJson(record.persona),
        record.sessionId ?? null,
        record.resolvedModel ?? null,
        now,
        now,
      );
  }

  getActor(runId: string, siteId: string, ordinal: number): ActorRecord | undefined {
    const row = this.db
      .prepare("select * from dwf_actor where run_id = ? and site_id = ? and ordinal = ?")
      .get(runId, siteId, ordinal) as DwfActorRow | undefined;
    return row ? decodeActor(row) : undefined;
  }

  listActors(runId: string): ActorRecord[] {
    const rows = this.db
      .prepare("select * from dwf_actor where run_id = ? order by id")
      .all(runId) as unknown as DwfActorRow[];
    return rows.map(decodeActor);
  }

  putNode(record: NodeRecord): void {
// Access (running) → Settlement → Statistics backfill all go through the same upsert, the key is (run_id, site_id, ordinal).
    const now = Date.now();
    this.db
      .prepare(
        `
        insert into dwf_node (
          run_id, site_id, ordinal, kind, actor_site_id, actor_ordinal, actor_seq,
          input_hash, status, result_json, error_json, stats_json, message_boundary,
          artifact_id, input_json, time_created, time_updated
        ) values (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        on conflict(run_id, site_id, ordinal) do update set
          kind = excluded.kind,
          actor_site_id = excluded.actor_site_id,
          actor_ordinal = excluded.actor_ordinal,
          actor_seq = excluded.actor_seq,
          input_hash = excluded.input_hash,
          status = excluded.status,
          result_json = excluded.result_json,
          error_json = excluded.error_json,
          stats_json = excluded.stats_json,
          message_boundary = excluded.message_boundary,
          artifact_id = excluded.artifact_id,
          input_json = excluded.input_json,
          time_updated = excluded.time_updated
        `,
      )
      .run(
        record.runId,
        record.siteId,
        record.ordinal,
        record.kind,
        record.actorSiteId ?? null,
        record.actorOrdinal ?? null,
        record.actorSeq ?? null,
        record.inputHash,
        record.status,
        encodeResultJson(record.result),
        encodeJson(record.error),
        encodeJson(record.stats),
// Complementary column of driver of the same family as stats_json: upsert replaces the entire column, so the supplementary party must first getNode
// Then rewrite the entire record (contract this read-write mode).
        record.messageBoundary ?? null,
// Product id and kind belong to the same ** engine, and the identity column of ** is included in both writes (access running and settlement
// completed/failed writes the same id): upsert is replaced in its entirety, so writing it once less is equivalent to replacing
// The product ownership of the settlement line is erased, and the post disappears directly from the reading page of "What products are there in this run?"
        record.artifactId ?? null,
// 0030: Bounded input and kind / artifact_id belong to the same column that "the engine writes twice" - upsert replaces the entire line.
// If the settlement is missed once, the op / args recorded in the admission will be erased back to NULL.
        encodeJson(record.input),
        now,
        now,
      );
  }

  getNode(runId: string, siteId: string, ordinal: number): NodeRecord | undefined {
    const row = this.db
      .prepare("select * from dwf_node where run_id = ? and site_id = ? and ordinal = ?")
      .get(runId, siteId, ordinal) as DwfNodeRow | undefined;
    return row ? decodeNode(row) : undefined;
  }

  listNodes(runId: string): NodeRecord[] {
    const rows = this.db
      .prepare("select * from dwf_node where run_id = ? order by id")
      .all(runId) as unknown as DwfNodeRow[];
    return rows.map(decodeNode);
  }

  listArtifactRows(runId: string): NodeRecord[] {
    return listArtifactRows(this.db, runId);
  }

  listWorldNodes(runId: string): DwfWorldNodeRow[] {
    return listWorldNodes(this.db, runId);
  }

  listArtifactItems(
    runId: string,
    artifactId: string,
    query: DwfArtifactItemsQuery,
  ): DwfArtifactItem[] {
    return listArtifactItems(this.db, runId, artifactId, query);
  }

  appendEvent(runId: string, event: RunEvent): StoredEvent {
// The moment when time_created is written, it must be returned to the caller as it is: when reading this line back, decodeEvent gives the same number.
// If you add a path but cannot get it, there will be an inconsistency like "the event just written does not have a time, but it will be after reading it back".
    const timeCreated = Date.now();
// Sequence number allocation and writing must be the same statement: MAX(sequence)+1. Reading it once and then inserting it will cause multiple problems in multiple processes.
// (zcode under WAL allows multiple Agents to share the same library) to compete for duplicate serial numbers.
    const row = this.db
      .prepare(
        `
        insert into dwf_event (run_id, sequence, type, payload_json, time_created)
        values (
          ?,
          coalesce((select max(sequence) + 1 from dwf_event where run_id = ?), 0),
          ?, ?, ?
        )
        returning sequence
        `,
      )
      .get(runId, runId, event.type, JSON.stringify(event), timeCreated) as
      | Pick<DwfEventRow, "sequence">
      | undefined;
    if (row === undefined) {
      throw new Error(`dwf journal: event insert returned no sequence for run: ${runId}`);
    }
    return { sequence: row.sequence, event, timeCreated };
  }

  listEvents(runId: string, opts?: ListEventsOptions): StoredEvent[] {
// Both cursor and limit are pushed down to SQL: take the full amount here and then slice it, which is equivalent to slicing the entire journal every time you turn a page.
// Read into memory - the reason paging exists is not to do this. Cursor semantics are "strictly greater than" (memory implementation is the same).
    const after = opts?.afterSequence;
    const limit = opts?.limit;
    const where = after === undefined ? "run_id = ?" : "run_id = ? and sequence > ?";
    const params: Array<string | number> = after === undefined ? [runId] : [runId, after];
// `limit -1` is SQLite's "unlimited" way of writing, so the default and explicit limit share the same statement shape.
    const rows = this.db
      .prepare(`select * from dwf_event where ${where} order by sequence limit ?`)
      .all(...params, limit === undefined ? -1 : Math.max(0, limit)) as unknown as DwfEventRow[];
    return rows.map(decodeEvent);
  }

/** UPDATE affects 0 rows i.e. "unknown run" - SQLite will not error for this, must be checked out explicitly and fail loudly. */
  private assertRunTouched(changes: number | bigint, runId: string): void {
    if (Number(changes) === 0) throw new Error(`dwf journal: unknown run: ${runId}`);
  }
}

/** Open a dwf journal view on the existing session library; the table is created by migration `0019_dwf_journal`. */
export function createDwfJournalStore(db: DatabaseSync): JournalStorePort {
  return new SqliteDwfJournalStore(db);
}
