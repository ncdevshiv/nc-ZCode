// Cargo（native/ 工作区）三方许可快照与新鲜度检查。
// 与 third-party/native-search/sources.json 同构：登记 Cargo.lock 的保守全集
// （含 dev/build/test 依赖，不声称每个 crate 都被每个平台链接），许可原文从
// crates.io 解包源码快照进 third-party/native-cargo/licenses/，再由
// scripts/generate-third-party-notices.mjs 内联进 THIRD-PARTY-NOTICES.md。
// 用法：
//   node scripts/third-party-cargo.mjs --write   重新生成快照
//   node scripts/third-party-cargo.mjs           校验快照与 Cargo.lock 一致
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdir, readFile, readdir, rm, writeFile } from "node:fs/promises";
import { dirname, join, relative } from "node:path";
import { pathToFileURL } from "node:url";

import { repositoryRoot } from "./third-party-notices.mjs";

export const cargoSourcesPath = "third-party/native-cargo/sources.json";
// 输入按 CRLF 归一后取哈希，与 inventory.json 的输入口径一致（Windows checkout 稳定）。
const CARGO_INPUTS = ["scripts/third-party-cargo.mjs", "native/Cargo.toml", "native/Cargo.lock"];
const NOTICE_FILE_RE = /^(LICENSE|LICENCE|COPYING|NOTICE|COPYRIGHT|UNLICENSE)([.-].*)?$/iu;

const hash = (bytes) => createHash("sha256").update(bytes).digest("hex");
const normalizedHash = (bytes) => hash(bytes.toString("utf8").replaceAll("\r\n", "\n"));
const repoPath = (file) => file.replaceAll("\\", "/");

function cargoMetadata(root) {
  const out = execFileSync(
    "cargo",
    [
      "metadata",
      "--format-version",
      "1",
      "--locked",
      "--manifest-path",
      join(root, "native", "Cargo.toml"),
    ],
    { encoding: "utf8", maxBuffer: 64 * 1024 * 1024 },
  );
  return JSON.parse(out);
}

/** 收集 native/ 工作区锁文件的全部三方包（排除工作区成员本身），确定性排序去重。 */
async function collectCargoPackages(root) {
  const metadata = cargoMetadata(root);
  const byId = new Map();
  for (const pkg of metadata.packages) {
    const manifestRel = repoPath(relative(root, pkg.manifest_path));
    if (manifestRel.startsWith("native/")) continue; // 工作区成员不是三方依赖
    const key = `${pkg.name}@${pkg.version}`;
    if (byId.has(key)) continue;
    byId.set(key, {
      name: pkg.name,
      version: pkg.version,
      license: typeof pkg.license === "string" && pkg.license.trim() ? pkg.license.trim() : null,
      source: pkg.id.startsWith("registry+") ? "crates.io registry" : pkg.id,
      manifestDir: dirname(pkg.manifest_path),
    });
  }
  return [...byId.values()].sort(
    (a, b) => a.name.localeCompare(b.name, "en") || a.version.localeCompare(b.version, "en"),
  );
}

/** 许可文件发现：包根目录的 LICENSE/COPYING/NOTICE 系列 + LICENSES/ 目录，basename 去重。 */
async function findNoticeFiles(dir) {
  const found = new Map(); // basename → source path
  let entries;
  try {
    entries = await readdir(dir, { withFileTypes: true });
  } catch {
    return [];
  }
  const consider = async (directory, prefix = "") => {
    let children;
    try {
      children = await readdir(directory, { withFileTypes: true });
    } catch {
      return;
    }
    for (const child of children) {
      if (!child.isFile()) continue;
      if (prefix === "" && !NOTICE_FILE_RE.test(child.name)) continue;
      if (prefix !== "" && !NOTICE_FILE_RE.test(child.name)) continue;
      const name = prefix ? `${prefix}-${child.name}` : child.name;
      if (!found.has(name)) found.set(name, join(directory, child.name));
    }
  };
  await consider(dir);
  for (const entry of entries) {
    if (entry.isDirectory() && entry.name.toUpperCase() === "LICENSES") {
      await consider(join(dir, entry.name), "LICENSES");
    }
  }
  return [...found.entries()].sort(([a], [b]) => a.localeCompare(b, "en"));
}

async function fileExists(file) {
  try {
    await readFile(file);
    return true;
  } catch (error) {
    if (error.code === "ENOENT") return false;
    throw error;
  }
}

/** --write：重建 third-party/native-cargo/（sources.json + 许可原文快照）。 */
export async function writeCargoNotices(root = repositoryRoot) {
  const packages = await collectCargoPackages(root);
  const licensesRoot = join(root, "third-party", "native-cargo", "licenses");
  await rm(licensesRoot, { recursive: true, force: true });
  const records = [];
  for (const pkg of packages) {
    const notices = [];
    for (const [name, sourcePath] of await findNoticeFiles(pkg.manifestDir)) {
      const bytes = await readFile(sourcePath);
      const destination = join(licensesRoot, `${pkg.name}-${pkg.version}`, name);
      await mkdir(dirname(destination), { recursive: true });
      await writeFile(destination, bytes);
      notices.push({ file: repoPath(relative(root, destination)), sha256: hash(bytes) });
    }
    const { manifestDir: _manifestDir, ...record } = pkg;
    records.push({ ...record, notices });
  }
  const inputs = {};
  for (const file of CARGO_INPUTS) {
    inputs[file] = normalizedHash(await readFile(join(root, file)));
  }
  const inventory = {
    schemaVersion: 1,
    scope:
      "Conservative union of the native/ Cargo.lock (dev/build/test crates included, optional/platform crates not claims of linkage). License texts are snapshotted from the extracted crates.io sources beside this file; exact hashes are recorded here.",
    inputs,
    packages: records,
  };
  await mkdir(join(root, "third-party", "native-cargo"), { recursive: true });
  await writeFile(join(root, cargoSourcesPath), `${JSON.stringify(inventory, null, 2)}\n`, "utf8");
  const missing = records.filter((item) => !item.notices.length).length;
  return { inventory, missing };
}

/**
 * 读取并（可选）校验快照；readInput 用于把所有输入并入 notices 生成器的哈希账本。
 * onNotice(bytes, owner, origin) 在生成模式下把许可原文并入去重文本表。
 * 返回 { packages }；cargo 工作区不存在时返回空集合。
 */
export async function readCargoNotices(
  root = repositoryRoot,
  { verify = false, readInput, onNotice } = {},
) {
  const read = async (file) => (readInput ? readInput(file) : readFile(join(root, file)));
  if (!(await fileExists(join(root, cargoSourcesPath)))) {
    if (await fileExists(join(root, "native", "Cargo.lock"))) {
      throw new Error(
        "Cargo notice snapshot missing while native/Cargo.lock exists; run node scripts/third-party-cargo.mjs --write",
      );
    }
    return { packages: [] };
  }
  const raw = await read(cargoSourcesPath);
  const inventory = JSON.parse(raw.toString("utf8"));
  if (inventory.schemaVersion !== 1) throw new Error("Unsupported cargo notice inventory");
  for (const [file, expected] of Object.entries(inventory.inputs)) {
    const bytes = await readFile(join(root, file));
    if (normalizedHash(bytes) !== expected)
      throw new Error(
        `Cargo notice input changed: ${file}; run node scripts/third-party-cargo.mjs --write`,
      );
  }
  for (const pkg of inventory.packages) {
    for (const notice of pkg.notices) {
      const bytes = await read(notice.file);
      const sha256 = hash(bytes);
      if (verify && sha256 !== notice.sha256)
        throw new Error(`Changed cargo notice: ${notice.file}`);
      onNotice?.(bytes, `${pkg.name}@${pkg.version}`, notice.file);
    }
  }
  return { packages: inventory.packages };
}

/** 独立门禁：快照必须与当前 Cargo.lock/源码一致，且无陈旧包记录。 */
export async function checkCargoNotices(root = repositoryRoot) {
  if (!(await fileExists(join(root, "native", "Cargo.lock")))) {
    console.log("[cargo-notices] no native/Cargo.lock — skipped");
    return;
  }
  await readCargoNotices(root, { verify: true });
  const inventory = JSON.parse(await readFile(join(root, cargoSourcesPath), "utf8"));
  const current = await collectCargoPackages(root);
  const currentSet = new Map(current.map((item) => [`${item.name}@${item.version}`, item]));
  const recordedSet = new Map(
    inventory.packages.map((item) => [`${item.name}@${item.version}`, item]),
  );
  const added = [...currentSet.keys()].filter((key) => !recordedSet.has(key));
  const removed = [...recordedSet.keys()].filter((key) => !currentSet.has(key));
  if (added.length || removed.length) {
    throw new Error(
      `Cargo notice package set drifted (+${added.join(", ")} / -${removed.join(", ")}); run node scripts/third-party-cargo.mjs --write`,
    );
  }
  // 源码目录内许可文件集合本身也要一致（覆盖"版本没变但快照缺文件"的情况）。
  for (const [key, pkg] of recordedSet) {
    const fresh = currentSet.get(key);
    const sourceHashes = new Set();
    for (const [, sourcePath] of await findNoticeFiles(fresh.manifestDir)) {
      sourceHashes.add(hash(await readFile(sourcePath)));
    }
    const recordedHashes = new Set(pkg.notices.map((notice) => notice.sha256));
    if (
      sourceHashes.size !== recordedHashes.size ||
      [...sourceHashes].some((sha) => !recordedHashes.has(sha))
    ) {
      throw new Error(
        `Cargo notice set drifted for ${key}; run node scripts/third-party-cargo.mjs --write`,
      );
    }
  }
  console.log(`[cargo-notices] OK: ${inventory.packages.length} crates, ${cargoSourcesPath} fresh`);
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  try {
    if (process.argv.includes("--write")) {
      const { inventory, missing } = await writeCargoNotices(repositoryRoot);
      console.log(
        `[cargo-notices] wrote ${cargoSourcesPath}: ${inventory.packages.length} crates, ${missing} without license snapshot`,
      );
    } else {
      await checkCargoNotices(repositoryRoot);
    }
  } catch (error) {
    console.error(`[cargo-notices] FAILED: ${error instanceof Error ? error.message : error}`);
    process.exit(1);
  }
}
