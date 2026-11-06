#!/usr/bin/env node
// Session read shape benchmark: full read vs tail window read.
//
// Mirror `apps/zcode-cli/packages/adapters/src/storage/session-store/repositories/messages.ts`
// The three SQLs and decoding order in `messages()` / `messagesTail()` / `hasUnorderedMessages()`,
// Only replace the type dependency of @zcode/contracts (this benchmark does not introduce the in-package runtime).
//
// Usage: node scripts/bench-session-read.mjs [Number of messages...]

import { DatabaseSync } from "node:sqlite";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const COUNTS = process.argv.slice(2).map(Number).filter(Number.isFinite);
const SIZES = COUNTS.length > 0 ? COUNTS : [2000, 8000, 32000];
const WINDOW = 60;

const dir = mkdtempSync(join(tmpdir(), "zcode-bench-session-read-"));
const db = new DatabaseSync(join(dir, "db.sqlite"));

db.exec(`
  pragma journal_mode = wal;
  create table message (
    id text primary key,
    session_id text not null,
    time_created integer not null,
    time_updated integer not null,
    data text not null,
    sequence integer
  );
  create index message_session_time_created_id_idx on message(session_id, time_created, id);
  create index message_session_sequence_idx on message(session_id, sequence, time_created, id);
  create table part (
    id text primary key,
    message_id text not null,
    session_id text not null,
    time_created integer not null,
    time_updated integer not null,
    data text not null,
    sequence integer
  );
  create index part_message_id_id_idx on part(message_id, id);
  create index part_session_idx on part(session_id);
  create index part_message_sequence_idx on part(message_id, sequence, time_created, id);
  create index part_session_message_sequence_idx on part(session_id, message_id, sequence);
`);

const SESSION = "ses_bench";
const messageData = JSON.stringify({role: "assistant",
  model: "glm-4.6",
  provider: "zai",
  tokens: { input: 12000, output: 800 },
  summary: "x".repeat(400),
});
const partData = JSON.stringify({
  type: "text",
  text: "y".repeat(600),
  metadata: { source: "assistant" },
});

function seed(count) {
  db.exec("delete from part; delete from message;");
  const insertMessage = db.prepare(
    "insert into message (id, session_id, time_created, time_updated, data, sequence) values (?,?,?,?,?,?)",
  );
  const insertPart = db.prepare(
    "insert into part (id, message_id, session_id, time_created, time_updated, data, sequence) values (?,?,?,?,?,?,?)",
  );
  db.exec("begin");
  const now = Date.now();
  let partSeq = 0;
  for (let m = 0; m < count; m += 1) {
    const messageId = `msg_${String(m).padStart(7, "0")}`;
    insertMessage.run(messageId, SESSION, now + m, now + m, messageData, m);
    for (let p = 0; p < 4; p += 1) {
      insertPart.run(`prt_${m}_${p}`, messageId, SESSION, now + m, now + m, partData, partSeq);
      partSeq += 1;
    }
  }
  db.exec("commit");
  db.exec("analyze");
}

const decode = (row) => ({ ...row, data: JSON.parse(row.data) });

//──Current status: Read in full (messages.ts:221)
function readAll(sessionID) {
  const messageRows = db
    .prepare(
      `select * from message where session_id = ?
       order by sequence is null, sequence, time_created, rowid`,
    )
    .all(sessionID);
  const partRows = db
    .prepare(
      `select * from part where session_id = ?
       order by message_id, sequence is null, sequence, time_created, id`,
    )
    .all(sessionID);
  const partsByMessage = new Map();
  for (const row of partRows) {
    const part = decode(row);
    const list = partsByMessage.get(row.message_id) ?? [];
    list.push(part);partsByMessage.set(row.message_id, list);
  }
  return messageRows.map((row) => ({ info: decode(row), parts: partsByMessage.get(row.id) ?? [] }));
}

// ── New: Tail window reading (messages.ts messagesTail)
function readTail(sessionID, limit) {
  const messageRows = db
    .prepare(
      `select * from message where session_id = ?
       order by sequence desc, time_created desc, rowid desc limit ?`,
    )
    .all(sessionID, limit);
  messageRows.reverse();
  if (messageRows.length === 0) return [];
  const placeholders = messageRows.map(() => "?").join(",");
  const partRows = db
    .prepare(
      `select * from part where session_id = ? and message_id in (${placeholders})
       order by message_id, sequence is null, sequence, time_created, id`,
    )
    .all(sessionID, ...messageRows.map((row) => row.id));
  const partsByMessage = new Map();
  for (const row of partRows) {
    const part = decode(row);
    const list = partsByMessage.get(row.message_id) ?? [];
    list.push(part);
    partsByMessage.set(row.message_id, list);
  }
  return messageRows.map((row) => ({ info: decode(row), parts: partsByMessage.get(row.id) ?? [] }));
}

// ── New: session/meta - only count, do not read the data column (messages.ts messageCounts)
function readMeta(sessionID) {
  const messageRow = db
    .prepare("select count(*) as total from message where session_id = ?")
    .get(sessionID);
  const partRow = db.prepare("select count(*) as total from part where session_id = ?").get(sessionID);
  return { messages: messageRow.total, parts: partRow.total };
}

function timeIt(fn, runs = 7) {
  fn();
  const samples = [];
  for (let i = 0; i < runs; i += 1) {
    const startedAt = performance.now();
    fn();
    samples.push(performance.now() - startedAt);
  }
  samples.sort((a, b) => a - b);  return samples[Math.floor(samples.length / 2)];
}

console.log(`window=${WINDOW} messages, 4 parts each\n`);
console.log("messages | full read | tail(60) | session/meta | speedup(full→tail) | rows read");

for (const count of SIZES) {
  seed(count);
  const full = timeIt(() => readAll(SESSION));
  const tail = timeIt(() => readTail(SESSION, WINDOW));
  const meta = timeIt(() => readMeta(SESSION));
  const rowsFull = count + count * 4;
  const rowsTail = WINDOW + WINDOW * 4;
  console.log(
    `${String(count).padStart(8)} | ${full.toFixed(1).padStart(7)}ms | ${tail
      .toFixed(1)
      .padStart(7)}ms | ${meta.toFixed(2).padStart(10)}ms | ${(full / tail)
      .toFixed(0)
      .padStart(17)}x | ${rowsFull} → ${rowsTail}`,
  );
}

db.close();
rmSync(dir, { recursive: true, force: true });
