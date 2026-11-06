// Shadow replay reconciliation (delivery/test infrastructure).
// Purpose: Feed all the historical sessions of the local real CLI library (default ~/.zcode/cli/db/db.sqlite)
// Cold recovery pipeline (transcript synthesis → ProductProjection), output conservation reconciliation report——
// Each stage's online threshold = full replay without crashes, no silent discards, and failure list review completed.
//
// Read-only for the source library: By default, the db (including -wal/-shm) is copied to the temporary directory and then opened - the store will run when it is opened.
// Migration (0015/0016, etc.) cannot fall directly on the user's real library (migration should be applied by the CLI normal startup path).
// Build before running: bun --cwd apps/zcode-cli build (script imports from each package dist).
//
// Usage:
//   node scripts/shadow-replay.mjs [--db <path>] [--limit <n>] [--session <id>] [--verbose] [--no-copy]
import { parseArgs } from "node:util";
import { homedir, tmpdir } from "node:os";
import { join, dirname, basename } from "node:path";
import { copyFileSync, existsSync, mkdtempSync } from "node:fs";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const pkg = (relative) =>
  import(pathToFileURL(join(here, "..", "packages", relative)).href);

const { values: args } = parseArgs({
  options: {
    db: { type: "string" },
    limit: { type: "string" },
    session: { type: "string" },
    verbose: { type: "boolean", default: false },
    "no-copy": { type: "boolean", default: false },
  },
});

const [{ createSqliteSessionStore }, hydration, projectionModule, contracts] =
  await Promise.all([
    pkg("adapters/dist/storage/index.js"),
    pkg("bootstrap/dist/zcode-protocol-v4/transcript-hydration.js"),
    pkg("bootstrap/dist/zcode-protocol-v4/product-projection.js"),
    pkg("contracts/dist/index.js"),
  ]);
const {
  synthesizeEventsFromMessages,
  goalVerificationEntriesFromSessionEntries,
} = hydration;
const { ProductProjection } = projectionModule;
const { SESSION_ENTRY_TARGET_COMPLETION_VERIFICATION } = contracts;

const sourceDbPath = args.db ?? join(homedir(), ".zcode", "cli", "db", "db.sqlite");
const limit = args.limit ? Number(args.limit) : Infinity;

let dbPath = sourceDbPath;
if (!args["no-copy"]) {
  const tempDir = mkdtempSync(join(tmpdir(), "zcode-shadow-replay-"));
  dbPath = join(tempDir, basename(sourceDbPath));
  copyFileSync(sourceDbPath, dbPath);
  for (const suffix of ["-wal", "-shm"]) {
    if (existsSync(sourceDbPath + suffix)) {
      copyFileSync(sourceDbPath + suffix, dbPath + suffix);
    }
  }
}

const store = createSqliteSessionStore({ dbPath });

const sessions = args.session
  ? [{ id: args.session }]
  : await store.listSessions({ includeArchived: true, limit: 100000 });

const totals = {
  sessions: 0,
  crashed: 0,
  assistantTextMissing: 0,
  userInputMismatch: 0,
  goalVerifyMissing: 0,
  modelChangeMissing: 0,
  entityTargetMismatch: 0,
  actionAddressabilityMismatch: 0,
  clean: 0,
};
const offenders = [];

for (const session of sessions) {
  if (totals.sessions >= limit) break;
  totals.sessions += 1;
  const sessionId = String(session.id);
  const report = {
    sessionId,
    problems: [],
  };
  try {
    const messages = await store.messages({ sessionID: session.id });
    if (messages.length === 0) continue;
    const entries = store.sessionEntries
      ? await store.sessionEntries({
          sessionID: session.id,
          type: SESSION_ENTRY_TARGET_COMPLETION_VERIFICATION,
        })
      : [];
    const goalVerificationEntries = goalVerificationEntriesFromSessionEntries(entries);
    const events = synthesizeEventsFromMessages(messages, {
      sessionId,
      goalVerificationEntries,
    });
    const projection = new ProductProjection(sessionId, "shadow-replay");
    for (const event of events) projection.applyEvent(event);
    const rows = projection.getSnapshot().rows.window;

//──Conservation reconciliation──
// assistant conservation: every visible assistant text must appear in rows (multiset coverage).
    const expectedTexts = [];
    for (const message of messages) {
      if (message.info.role !== "assistant") continue;
      for (const part of message.parts) {
        if (part.type === "text" && part.ignored !== true && part.text.length > 0) {
          expectedTexts.push(part.text);
        }
      }
    }
    const actualTexts = rows
      .filter((row) => row.kind === "assistantText")
      .map((row) => row.text);
    const remaining = [...actualTexts];
    let missingTexts = 0;
    for (const text of expectedTexts) {
      const index = remaining.indexOf(text);
      if (index < 0) missingTexts += 1;
      else remaining.splice(index, 1);
    }
    if (missingTexts > 0) {
      totals.assistantTextMissing += 1;
      report.problems.push(`assistant text missing ${missingTexts}/${expectedTexts.length}`);
    }

// goal verify conservation: the number of lifecycle keys of part ∪ entry vs the number of goalVerify marker rows.
    const goalKeys = new Set();
    for (const message of messages) {
      for (const part of message.parts) {
        if (part.type === "timeline" && part.timelineType === "goal_verification") {
          goalKeys.add(
            part.goalIteration !== undefined
              ? `${part.targetId}_${part.goalIteration}`
              : part.verificationId,
          );
        }
      }
    }
    for (const entry of goalVerificationEntries) {
      const payload = entry.payload;
      goalKeys.add(
        payload.goalIteration !== undefined
          ? `${payload.targetId}_${payload.goalIteration}`
          : payload.verificationId,
      );
    }
    const goalMarkers = rows.filter(
      (row) => row.kind === "timelineMarker" && row.marker.type === "goalVerify",
    ).length;
    if (goalMarkers < goalKeys.size) {
      totals.goalVerifyMissing += 1;
      report.problems.push(`goalVerify marker ${goalMarkers}/${goalKeys.size}`);
    }

// Visible user input conservation: the number of userInput lines must not be less than the lower limit estimate of the number of real user text messages
// (synthetic/model-only does not count; guide steer inline is also a visible line and counts both sides).
// Known false positive: goal-continuation reminder / fork notice in legacy data as bare text
// Persistence (no synthetic/source/metadata tag), classifiers are correctly hidden by text prefix,
// This heuristic will count them as visible - flag needs to be checked manually (21 in full replay
// The flag is all false positives of this type, and the true conservation failure is 0).
    const expectedUsers = messages.filter(
      (message) =>
        message.info.role === "user" &&
        message.info.synthetic !== true &&
        message.info.visibility !== "model-only" &&
// compact continuation summary (info.summary / semantics.kind=compact_summary) is
// providerContextOnly: The classifier ruling is not visible and does not count towards the lower limit of visible users.
        message.info.summary === undefined &&
        !message.info.semantics?.kind?.startsWith?.("compact") &&
        message.info.source === undefined &&
        message.parts.some(
          (part) => part.type === "text" && part.ignored !== true && part.text.length > 0,
        ),
    ).length;
    const actualUsers = rows.filter((row) => row.kind === "userInput").length;
    if (actualUsers < expectedUsers) {
      totals.userInputMismatch += 1;
      report.problems.push(`userInput line ${actualUsers}/${expectedUsers}`);
    }

// Addressability conservation: shadow replay cannot just prove "the text is still there". of each transcript row
// entity/productTurn/message target, and each published action, must be started by the same cold
// The resolver of materialization is an exact hit; otherwise the UI will display the button but the command must be stale/reject.
    let entityTargetProblems = 0;
    let actionProblems = 0;
    const actionByFlag = [
      ["canEdit", "editUserQuery"],
      ["canRetry", "retryTurn"],
      ["canFork", "forkAssistant"],
      ["canRewindFiles", "fileRewindPreview"],
    ];
    for (const row of rows) {
      const entityId = projection.getEntityIdForRow(row.rowId);
      const messageId = projection.getMessageIdForRow(row.rowId);
      if (
        (row.kind === "userInput" || row.kind === "assistantText") &&
        (!entityId || !messageId || !row.turnId)
      ) {
        entityTargetProblems += 1;
      }
      for (const [flag, action] of actionByFlag) {
        if (row.actions?.[flag] !== true) continue;
        if (!entityId) {
          actionProblems += 1;
          continue;
        }
        const resolution = projection.resolveRowActionTarget(
          { rowId: row.rowId, entityId },
          action,
        );
        if (!resolution.ok || resolution.row.turnId !== row.turnId) {
          actionProblems += 1;
          continue;
        }
        if (
          (action === "editUserQuery" ||
            action === "retryTurn" ||
            action === "forkAssistant") &&
          (!messageId ||
            ("messageId" in resolution && resolution.messageId !== messageId) ||
            ("editTarget" in resolution &&
              resolution.editTarget.productTurnId !== row.turnId))
        ) {
          actionProblems += 1;
        }
      }
    }
    if (entityTargetProblems > 0) {
      totals.entityTargetMismatch += 1;
      report.problems.push(`entity/target is not addressable ${entityTargetProblems}`);
    }
    if (actionProblems > 0) {
      totals.actionAddressabilityMismatch += 1;
      report.problems.push(`row.actions resolver is not equivalent to ${actionProblems}`);
    }

    if (report.problems.length === 0) totals.clean += 1;
  } catch (error) {
    totals.crashed += 1;
    report.problems.push(`Replay crash: ${error?.message ?? error}`);
  }
  if (report.problems.length > 0) {
    offenders.push(report);
    if (args.verbose) {
      console.log(`✗ ${sessionId}: ${report.problems.join("; ")}`);
    }
  }
}

store.close();

console.log("\n── Shadow replay reconciliation report──");
console.log(`Source DB: ${sourceDbPath}${dbPath === sourceDbPath ? "" : "(copied to temporary copy for replay)"}`);
console.log(`Sessions: ${totals.sessions} (clean ${totals.clean})`);
console.log(`重放崩溃: ${totals.crashed}`);
console.log(`assistant text 缺失: ${totals.assistantTextMissing}`);
console.log(`userInput 行不足: ${totals.userInputMismatch}`);
console.log(`goalVerify marker 缺失: ${totals.goalVerifyMissing}`);
console.log(`entity/target 不可寻址: ${totals.entityTargetMismatch}`);
console.log(`row.actions resolver 不等价: ${totals.actionAddressabilityMismatch}`);
if (offenders.length > 0 && !args.verbose) {
  console.log(`\n问题会话（前 20，--verbose 看全量）：`);
  for (const report of offenders.slice(0, 20)) {
    console.log(`  ${report.sessionId}: ${report.problems.join("; ")}`);
  }
}
process.exitCode =
  totals.crashed > 0 ||
  totals.entityTargetMismatch > 0 ||
  totals.actionAddressabilityMismatch > 0
    ? 1
    : 0;
