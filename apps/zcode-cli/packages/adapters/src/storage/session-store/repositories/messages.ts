import type { DatabaseSync } from "node:sqlite";
import type {
  MessageId,
  MessageInfo,
  MessagePart,
  MessageWithParts,
  PartId,
  SessionId,
  SessionStorePort,
} from "@zcode/contracts";
import { decodeMessageRow, decodePartRow, partCreatedAt } from "../codecs.js";
import { encodeJson } from "../json.js";
import type { MessageRow, PartRow } from "../rows.js";
import { touchSession } from "./sessions.js";

// Only replace the old snapshot fields, do not merge the entire JSON: recursive merge will leave the cleared metadata/options.
function preserveLegacyMembers(table: "message" | "part", keys: readonly string[]): string {
  return keys.reduce(
    (sql, key) =>
      `(select case when json_type(${table}.data, '$.${key}') is not null
      then json_set(previous.data, '$.${key}', json_extract(${table}.data, '$.${key}')) else previous.data end
      from (select ${sql} as data) as previous)`,
    "excluded.data",
  );
}

const MESSAGE_DATA_UPDATE = preserveLegacyMembers("message", [
  "model",
  "providerID",
  "modelID",
  "variant",
]);
const PART_DATA_UPDATE = preserveLegacyMembers("part", ["fromModel", "toModel", "model"]);

// on-conflict sequence rules:
// Saving with the same scope must retain the existing sequence as it is - including NULL. coalesce(existing, excluded)
// NULL historical rows (migration omissions/old version binary writes) will be rearranged to the end of the queue (excluded=max+1) when saving again,
// The second save after the assistant is completed is enough to trigger, causing the timeline to drift; NULL is fixed uniformly by 0015 backfill.
// Cross-scope binding changes (ID conflicts but session/message are different, only the upsert scenario is imported) will take the tail of the excluded new scope.
export async function saveMessage(
  db: DatabaseSync,
  input: MessageInfo,
  copyFrom?: Parameters<SessionStorePort["saveMessage"]>[1],
): Promise<void> {
  const { id, sessionID, ...data } = input;
// Freeze the old protocol mapper to read user.model unconditionally; missing the entire object will make the text unable to be opened.
// Complement only required objects; the original model of the old row is still retained by the conflicting update/copy logic and is not used by the new Reader.
  const storedData =
    input.role === "user"
      ? {
          ...data,
          model: input.modelSelection
            ? {
                providerID: input.modelSelection.providerId,
                modelID: input.modelSelection.modelId,
                ...(input.modelSelection.options?.reasoningLevel
                  ? { variant: input.modelSelection.options.reasoningLevel }
                  : {}),
              }
            : {},
        }
      : data;
  const timeCreated = input.time.created;
  const timeUpdated =
    input.role === "assistant" ? (input.time.completed ?? Date.now()) : timeCreated;

  db.prepare(
    `
      insert into message (id, session_id, time_created, time_updated, data, sequence)
      values (
        ?,
        ?,
        ?,
        ?,
        ?,
        (
          select coalesce(max(sequence), -1) + 1
          from message
          where session_id = ?
        )
      )
      on conflict(id) do update set
        session_id = excluded.session_id,
        time_updated = excluded.time_updated,
        -- 旧字段是回滚快照，不能因新版 Reader 隐藏了它们而在普通更新时丢掉。
        data = case when message.session_id = excluded.session_id then ${MESSAGE_DATA_UPDATE} else excluded.data end,
        sequence = case
          when message.session_id = excluded.session_id then message.sequence
          else excluded.sequence
        end
      `,
  ).run(
    id,
    sessionID,
    timeCreated,
    timeUpdated,
    encodeJson(copyLegacyMembers(db, "message", storedData, copyFrom)),
    sessionID,
  );
  touchSession(db, sessionID, timeUpdated);
}

export async function removeMessage(
  db: DatabaseSync,
  input: { sessionID: SessionId; messageID: MessageId },
): Promise<void> {
  db.prepare("delete from message where id = ? and session_id = ?").run(
    input.messageID,
    input.sessionID,
  );
}

export async function savePart(
  db: DatabaseSync,
  input: MessagePart,
  copyFrom?: Parameters<SessionStorePort["savePart"]>[1],
): Promise<void> {
  const { id, sessionID, messageID, ...data } = input;
  let storedData: Record<string, unknown> = data;
  if (input.type === "timeline" && input.timelineType === "model_change") {
    const { fromModel, toModel, ...part } = data as typeof input;
// Freeze the old version of Reader to directly access toModel.providerID; only do minimal compatible writes for this required object.
// The current Reader does not read it. When updating the same old row, SQL retains the original snapshot and does not continue reverse synchronization.
    const oldToModel = toModel
      ? {
          providerID: toModel.providerId,
          modelID: toModel.modelId,
          ...(toModel.options?.reasoningLevel ? { variant: toModel.options.reasoningLevel } : {}),
          label: toModel.label,
        }
      : {};
    storedData = {
      ...part,
      toModel: oldToModel,
      fromModelSelection: fromModel,
      toModelSelection: toModel,
    };
  } else if (input.type === "subtask") {
    const { model, ...part } = data as typeof input;
    storedData = { ...part, modelSelection: model };
  }
  const now = Date.now();

  db.prepare(
    `
      insert into part (id, message_id, session_id, time_created, time_updated, data, sequence)
      values (
        ?,
        ?,
        ?,
        ?,
        ?,
        ?,
        (
          select coalesce(max(sequence), -1) + 1
          from part
          where message_id = ?
        )
      )
      on conflict(id) do update set
        message_id = excluded.message_id,
        session_id = excluded.session_id,
        time_updated = excluded.time_updated,
        data = case when part.message_id = excluded.message_id and part.session_id = excluded.session_id
          then ${PART_DATA_UPDATE} else excluded.data end,
        sequence = case
          when part.message_id = excluded.message_id and part.session_id = excluded.session_id
            then part.sequence
          else excluded.sequence
        end
      `,
  ).run(
    id,
    messageID,
    sessionID,
    partCreatedAt(input, now),
    now,
    encodeJson(copyLegacyMembers(db, "part", storedData, copyFrom)),
    messageID,
  );
  touchSession(db, sessionID, now);
}

function copyLegacyMembers(
  db: DatabaseSync,
  table: "message" | "part",
  data: Record<string, unknown>,
  source: Parameters<SessionStorePort["saveMessage"]>[1],
): Record<string, unknown> {
  if (!source) return data;
// Replication only moves storage snapshots; the runtime layer does not read old fields, nor can it reconstruct old values ​​based on current selections.
  const row = db
    .prepare(`SELECT data FROM ${table} WHERE id=? AND session_id=?`)
    .get(source.id, source.sessionID);
  if (!row) throw new Error(`Storage copy source missing: ${table}/${source.id}`);
  const original = JSON.parse(String(row.data)) as Record<string, unknown>;
  const result = { ...data };
  const keys =
    table === "message"
      ? ["model", "providerID", "modelID", "variant"]
      : ["fromModel", "toModel", "model"];
  for (const key of keys) if (Object.hasOwn(original, key)) result[key] = original[key];
  return result;
}

export async function removePart(
  db: DatabaseSync,
  input: {
    sessionID: SessionId;
    messageID: MessageId;
    partID: PartId;
  },
): Promise<void> {
  db.prepare("delete from part where id = ? and message_id = ? and session_id = ?").run(
    input.partID,
    input.messageID,
    input.sessionID,
  );
}

export async function messages(
  db: DatabaseSync,
  input: { sessionID: SessionId },
): Promise<MessageWithParts[]> {
  const messageRows = db
    .prepare(
      `
      select * from message
      where session_id = ?
      order by sequence is null, sequence, time_created, rowid
      `,
    )
    .all(input.sessionID) as unknown as MessageRow[];

  const partRows = db
    .prepare(
      `
      select * from part
      where session_id = ?
      order by message_id, sequence is null, sequence, time_created, id
      `,
    )
    .all(input.sessionID) as unknown as PartRow[];
  const partsByMessage = new Map<string, MessagePart[]>();

  for (const row of partRows) {
    const part = decodePartRow(row);
    const list = partsByMessage.get(row.message_id) ?? [];
    list.push(part);
    partsByMessage.set(row.message_id, list);
  }

  return messageRows.map((row) => ({
    info: decodeMessageRow(row),
    parts: partsByMessage.get(row.id) ?? [],
  }));
}

export async function messageWithParts(
  db: DatabaseSync,
  input: { sessionID: SessionId; messageID: MessageId },
): Promise<MessageWithParts | null> {
  const messageRow = db
    .prepare("select * from message where id = ? and session_id = ?")
    .get(input.messageID, input.sessionID) as unknown as MessageRow | undefined;
  if (!messageRow) return null;

  const partRows = db
    .prepare(
      `
      select * from part
      where message_id = ? and session_id = ?
      order by sequence is null, sequence, time_created, id
      `,
    )
    .all(input.messageID, input.sessionID) as unknown as PartRow[];
  return {
    info: decodeMessageRow(messageRow),
    parts: partRows.map(decodePartRow),
  };
}
