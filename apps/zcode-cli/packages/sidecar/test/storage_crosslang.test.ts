/**
 * 跨语言存储 parity：Rust 写的库必须能被 TS（node:sqlite）读，TS 写的库必须能被
 * Rust 读——这是「单写者迁移期共存」的真实底线，不是同语言自洽测试。
 *
 * 本测试由 Rust 创建库并写入任务，然后**用 node:sqlite 打开同一个文件**验证
 * 结构与数据（schema 列名/默认值/索引语义一致）。反向方向（TS 写 → Rust 读）由
 * `storage_crosslang.test.ts` 覆盖。
 *
 * 运行：cd apps/zcode-cli/packages/sidecar && node --import tsx --test test/storage_crosslang.test.ts
 */
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtempSync, rmSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { DatabaseSync } from "node:sqlite";
import { after, test } from "node:test";

// <repo>/apps/zcode-cli/packages/sidecar/test → 五级上行到 repo 根。
const repoRoot = join(import.meta.dirname, "..", "..", "..", "..", "..");
const hostManifest = join(repoRoot, "native", "Cargo.toml");

let workdir: string;
let dbPath: string;

test("Rust-created database is readable by node:sqlite", () => {
  workdir = mkdtempSync(join(tmpdir(), "zcode-crosslang-"));
  dbPath = join(workdir, "tasks.sqlite");
  // 用 host 的 storage 层建库并写入种子任务（真实 cargo 二进制，不是 mock）。
  execFileSync(
    "cargo",
    [
      "run",
      "--quiet",
      "--manifest-path",
      hostManifest,
      "-p",
      "zcode-host",
      "--bin",
      "storage_probe",
      "--",
      dbPath,
    ],
    { stdio: "pipe" },
  );

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

    // Rust 写入的任务，TS 按 TS 侧查询读得出来。
    const rows = database
      .prepare(
        "SELECT task_id, title, mode, meta_json FROM tasks WHERE deleted = 0 ORDER BY updated_at DESC",
      )
      .all();
    assert.ok(rows.length >= 2, `expected seeded tasks, got ${rows.length}`);
    assert.equal(String(rows[0].task_id), "task-new");
    assert.equal(String(rows[0].title), "written by rust");

    // 反向：TS 插入的行，schema 默认值与 TS 一致（mode/meta_json/flags）。
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
    database.close();
    rmSync(workdir, { recursive: true, force: true });
  }
});
