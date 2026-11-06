import type { DatabaseSync } from "node:sqlite";
import {
  SESSION_ENTRY_MODEL_SELECTION,
  type SessionEntryInfo,
  type SessionEntryType,
  type SessionId,
} from "@zcode/contracts";
import { decodeSessionEntryRow } from "../codecs.js";
import { encodeJson } from "../json.js";
import type { SessionEntryRow } from "../rows.js";
import { touchSession } from "./sessions.js";

export function saveSessionEntry(db: DatabaseSync, input: SessionEntryInfo): void {
  const isModelSelection = input.type === SESSION_ENTRY_MODEL_SELECTION;
// The old version reads the flat identity of data; the new version of the port still passes the logical Selection, and the packaging only belongs to the storage adapter.
// undefined and null in existing fork are written as clear null values ​​to prevent restarting and re-importing old fields.
  const encoded = encodeJson(
    isModelSelection ? { modelSelection: input.data ?? null } : input.data,
  );
  if (!encoded) {
    throw new Error("Session entry data must be JSON-serializable");
  }

  db.prepare(
    `
      insert into session_entry (id, session_id, type, time_created, time_updated, data)
      values (?, ?, ?, ?, ?, ?)
      on conflict(id) do update set
        session_id = excluded.session_id,
        type = excluded.type,
        time_updated = excluded.time_updated,
        data = case
          when ? and session_entry.type = excluded.type
            and session_entry.session_id = excluded.session_id
            and json_type(session_entry.data) = 'object'
          then json_set(session_entry.data, '$.modelSelection', json_extract(excluded.data, '$.modelSelection'))
          else excluded.data
        end
      `,
  ).run(
    input.id,
    input.sessionID,
    input.type,
    input.time.created,
    input.time.updated,
    encoded,
    Number(isModelSelection),
  );
  if (input.touchSession !== false) {
    touchSession(db, input.sessionID, input.time.updated);
  }
}

export function sessionEntries(
  db: DatabaseSync,
  input: { sessionID: SessionId; type?: SessionEntryType | string },
): SessionEntryInfo[] {
  const rows = input.type
    ? db
        .prepare(
          `
          select * from session_entry
          where session_id = ? and type = ?
          order by time_created, rowid
          `,
        )
        .all(input.sessionID, input.type)
    : db
        .prepare(
          `
          select * from session_entry
          where session_id = ?
          order by time_created, rowid
          `,
        )
        .all(input.sessionID);

  return (rows as unknown as SessionEntryRow[]).map(decodeSessionEntryRow);
}
