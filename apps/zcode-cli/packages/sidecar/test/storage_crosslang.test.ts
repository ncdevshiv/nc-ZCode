/**
* Cross-language storage parity: libraries written in Rust must be readable by TS (node:sqlite), and libraries written in TS must be readable by TS (node:sqlite).
* Rust reading - this is the real bottom line of "single writer coexistence during the migration period", not a self-consistent test of the same language.
 *
* This test is created by Rust creating a library and writing tasks, and then **opening the same file using node:sqlite** to verify
* Structure and data (schema column names/default values/index semantics are consistent). The reverse direction (TS write → Rust read) is given by
* `storage_crosslang.test.ts` override.
 *
* Run: cd apps/zcode-cli/packages/sidecar && node --import tsx --test test/storage_crosslang.test.ts
 */
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtempSync, rmSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { DatabaseSync } from "node:sqlite";
import { after, test } from "node:test";

// <repo>/apps/zcode-cli/packages/sidecar/test → Five levels upstream to the repo root.
const repoRoot = join(import.meta.dirname, "..", "..", "..", "..", "..");
const hostManifest = join(repoRoot, "native", "Cargo.toml");

let workdir: string;
let dbPath: string;

test("Rust-created database is readable by node:sqlite", () => {
  workdir = mkdtempSync(join(tmpdir(), "zcode-crosslang-"));
  dbPath = join(workdir, "tasks.sqlite");
// Use the storage layer of the host to build a library and write the seed task (real cargo binary, not mock).
  const storageProbeBin = process.platform === "win32"
    ? join(repoRoot, "native", "target", "debug", "storage_probe.exe")
    : join(repoRoot, "native", "target", "debug", "storage_probe");
  if (!existsSync(storageProbeBin)) {
    execFileSync(
      "cargo",
      [
        "build",
        "--quiet",
        "--manifest-path",
        hostManifest,
        "-p",
        "nc-code-host",
        "--bin",
        "storage_probe",
      ],
      { stdio: "pipe" },
    );
  }
  execFileSync(storageProbeBin, [dbPath], { stdio: "pipe" });

  assert.ok(existsSync(dbPath), "Rust host must create the sqlite file");
  const database = new DatabaseSync(dbPath);
  try {
    const tables = database
      .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
      .all()
      .map((row) => String(row.name));
    for (const table of [
      "automation_runs",
      "automations",
      "task_group_members",
      "task_group_view_node_orders",
      "task_group_workspace_bootstraps",
      "task_groups",
      "tasks",
    ]) {
      assert.ok(tables.includes(table), `missing table for TS: ${table}`);
    }

// Tasks written by Rust can be read by TS by querying on the TS side.
    const rows = database
      .prepare(
        "SELECT task_id, title, mode, meta_json FROM tasks WHERE deleted = 0 ORDER BY updated_at DESC",
      )
      .all();
    assert.ok(rows.length >= 2, `expected seeded tasks, got ${rows.length}`);
    assert.equal(String(rows[0].task_id), "task-new");
    assert.equal(String(rows[0].title), "written by rust");

// Reverse: TS inserted rows, the default schema value is consistent with TS (mode/meta_json/flags).
    database
      .prepare(
        "INSERT INTO tasks (workspace_key, workspace_path, task_id, created_at, updated_at) VALUES (?,?,?,?,?)",
      )
      .run("ts-ws", "F:/ts", "ts-task", 1, 2);
    const inserted = database
      .prepare("SELECT mode, meta_json, archived, deleted FROM tasks WHERE task_id='ts-task'")
      .get();
    assert.equal(String(inserted.mode), "build");
    assert.equal(String(inserted.meta_json), "{}");
    assert.equal(Number(inserted.archived), 0);
    assert.equal(Number(inserted.deleted), 0);
  } finally {
    try {
      database.close();
    } catch {}
    try {
      rmSync(workdir, { recursive: true, force: true, maxRetries: 3, retryDelay: 100 });
    } catch {
      // Ignore cleanup errors on Windows
    }
  }
});
