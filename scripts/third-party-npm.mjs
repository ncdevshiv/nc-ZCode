import { execFile } from "node:child_process";
import { createHash } from "node:crypto";
import { readFile, readdir, realpath } from "node:fs/promises";
import { join, relative } from "node:path";
import { promisify } from "node:util";
import { resolveSpawnRuntimeOptions } from "./spawn-command.mjs";

const exec = promisify(execFile);
export const hashBytes = (bytes) => createHash("sha256").update(bytes).digest("hex");
const unsupportedCanvas = new Set([
  "@napi-rs/canvas-android-arm64",
  "@napi-rs/canvas-linux-arm-gnueabihf",
  "@napi-rs/canvas-linux-riscv64-gnu",
]);
const noticeName =
  /(?:^|[._-])(?:licen[sc]es?|copying|notice|copyright|unlicense|third.party|ofl)(?:[._-]|$)/iu;

export async function readPackageNotices(directory) {
  const files = [];
  async function visit(path) {
    for (const entry of await readdir(path, { withFileTypes: true })) {
      if (["node_modules", ".git", "test", "tests", "fixtures"].includes(entry.name)) continue;
      // Chromium 聚合许可由打包流程经 resources/licenses/electron 单独分发，不进 npm 通知。
      if (entry.isFile() && entry.name === "LICENSES.chromium.html") continue;
      const full = join(path, entry.name);
      if (entry.isDirectory()) await visit(full);
      else if (entry.isFile() && noticeName.test(entry.name)) {
        const bytes = await readFile(full);
        if (bytes.includes(0) || bytes.length === 0) continue;
        files.push({ member: relative(directory, full).replaceAll("\\", "/"), bytes });
      } else if (entry.isFile() && /^readme(?:\.[^/]*)?$/iu.test(entry.name)) {
        const text = await readFile(full, "utf8");
        const match = /^(?:#{1,6}\s+licen[sc]e[^\n]*\n|licen[sc]e\s*\n[=-]+\n)/imu.exec(text);
        if (match) {
          const section = text.slice(match.index).split(/\n(?=#{1,6}\s)/u)[0];
          files.push({
            member: `${relative(directory, full).replaceAll("\\", "/")} (license section)`,
            bytes: Buffer.from(section),
          });
        }
      }
    }
  }
  await visit(directory);
  return files.sort((a, b) => a.member.localeCompare(b.member, "en"));
}

function productionPackages(projects) {
  const own = new Set(projects.map((project) => project.name));
  const required = new Map();
  function dependencies(deps) {
    for (const [alias, info] of Object.entries(deps ?? {})) {
      const name = info.name ?? alias;
      if (!own.has(name) && !name.startsWith("@zcode/") && !info.version.startsWith("link:")) {
        required.set(`${name}@${info.version}`, { name, version: info.version });
      }
      dependencies(info.dependencies);
      dependencies(info.optionalDependencies);
    }
  }
  for (const project of projects) {
    dependencies(project.dependencies);
    dependencies(project.optionalDependencies);
  }
  return required;
}

export function assertProductionGraphs(lockedProjects, installedProjects) {
  const locked = productionPackages(lockedProjects);
  const installed = productionPackages(installedProjects);
  const missing = [...locked].filter(
    ([key, item]) => !installed.has(key) && !unsupportedCanvas.has(item.name),
  );
  const stale = [...installed.keys()].filter((key) => !locked.has(key));
  if (missing.length || stale.length) {
    throw new Error(
      `Installed production graph differs from pnpm-lock.yaml. Run pnpm install --frozen-lockfile.\nMissing: ${missing.map(([key]) => key).join(", ")}\nStale: ${stale.join(", ")}`,
    );
  }
  return locked;
}

/**
 * 展开 `pnpm-workspace.yaml` 的 `packages:` 为确定性的项目目录列表（含根项目）。
 * 不支持的 glob 形态直接抛错（fail-closed：宁可失败，禁止静默漏项目导致声明缺包）。
 */
async function listWorkspaceProjectDirs(root) {
  const text = await readFile(join(root, "pnpm-workspace.yaml"), "utf8");
  const patterns = [];
  let inPackages = false;
  for (const rawLine of text.split(/\r?\n/)) {
    const line = rawLine.trim();
    if (line === "" || line.startsWith("#")) continue;
    if (line === "packages:") {
      inPackages = true;
      continue;
    }
    if (!inPackages) continue;
    const item = /^-\s+(.*)$/.exec(line);
    if (item) patterns.push(item[1].trim().replace(/^(['"])(.*)\1$/, "$2"));
    else inPackages = false; // packages 段结束（遇到下一个顶层 key）
  }
  if (patterns.length === 0) throw new Error("pnpm-workspace.yaml 缺少 packages 列表");
  const dirs = new Set([root]);
  for (const pattern of patterns) {
    if (pattern.startsWith("!")) throw new Error(`pnpm-workspace 排除式 glob 未支持: ${pattern}`);
    if (pattern.endsWith("/*")) {
      const base = join(root, pattern.slice(0, -2));
      let entries;
      try {
        entries = await readdir(base, { withFileTypes: true });
      } catch (error) {
        if (error.code === "ENOENT") continue; // glob 基目录不存在 = 零匹配（与 pnpm 语义一致）
        throw error;
      }
      // 按名排序保证跨平台确定性（readdir 顺序不作承诺）。
      for (const entry of entries.sort((a, b) => a.name.localeCompare(b.name, "en"))) {
        if (!entry.isDirectory()) continue;
        const dir = join(base, entry.name);
        try {
          await readFile(join(dir, "package.json"), "utf8");
          dirs.add(dir);
        } catch (error) {
          if (error.code !== "ENOENT" && error.code !== "ENOTDIR") throw error;
        }
      }
    } else if (pattern.includes("*") || pattern.includes("?")) {
      throw new Error(`pnpm-workspace glob 形态未支持: ${pattern}`);
    } else {
      dirs.add(join(root, pattern));
    }
  }
  return [...dirs];
}

/** 按固定并发分批执行，保持输入顺序（避免一次性拉起全部 pnpm 进程）。 */
async function inBatches(items, size, run) {
  const out = [];
  for (let index = 0; index < items.length; index += size) {
    out.push(...(await Promise.all(items.slice(index, index + size).map(run))));
  }
  return out;
}

export async function readWorkspaceProductionGraph(root) {
  root = await realpath(root);
  // 修复：pnpm ls 默认读取安装快照，不能把旧图与当前锁文件哈希拼成有效声明。
  //
  // 修复（Windows 单进程句柄上限）：`pnpm -r ls --depth Infinity` 的递归 walk 跨整个
  // 工作区累积未关闭的文件句柄，撞上 Node/libuv 单进程 ~8192 并发句柄天花板必然 EMFILE
  // （本机实测：同一命令根单项目 exit 0、`-r` 连 --depth 0 都失败且失败文件随运行漂移，
  // 而 open→close 循环 30 万次无错——根因是 walk 规模下句柄未及时释放，不是句柄分配）。
  // 修复（嵌套 workspace 正确性）：`apps/zcode-cli` 自带 pnpm-workspace.yaml/lock，
  // 以子项目目录为 cwd 会解析到嵌套工作区，其 installed 图为空（实测 ACT deps=0）、与
  // lockfile 图不对称。两个 lockfile-only 变体必须同源：一律 cwd=workspace 根 +
  // `--filter <相对路径>` 只圈定单项目（实测 adapters 两侧均 21 deps 一致），每次新进程
  // 重置句柄预算，JSON 按 `-r` 的“各项目结果数组拼接”语义合并后仍做全量交叉校验。
  const projectDirs = await listWorkspaceProjectDirs(root);
  async function collect(lockfileOnly) {
    const collected = await inBatches(projectDirs, 6, async (dir) => {
      const scope =
        dir === root ? [] : ["--filter", `./${relative(root, dir).replaceAll("\\", "/")}`];
      const { stdout } = await exec(
        "pnpm",
        [
          ...scope,
          "ls",
          "--prod",
          "--json",
          "--depth",
          "Infinity",
          ...(lockfileOnly ? ["--lockfile-only"] : []),
        ],
        {
          cwd: root,
          maxBuffer: 256 * 1024 * 1024,
          ...resolveSpawnRuntimeOptions("pnpm"),
        },
      );
      return JSON.parse(stdout);
    });
    return collected.flat();
  }
  const locked = await collect(true);
  const actual = await collect(false);
  const required = assertProductionGraphs(locked, actual);
  return { required, projects: actual };
}

export async function scanInstalledPackages(root, projects) {
  // pnpm hoisted 布局的 ls.path 仍可能指向不存在的 .pnpm 路径；按真实安装目录和精确版本匹配。
  const installed = new Map();
  const visited = new Set();
  async function scanNodeModules(directory) {
    let actual;
    try {
      actual = await realpath(directory);
    } catch (error) {
      if (error.code === "ENOENT") return;
      throw error;
    }
    if (visited.has(actual)) return;
    visited.add(actual);
    const entries = await readdir(directory, { withFileTypes: true });
    for (const entry of entries) {
      if (entry.name.startsWith(".")) continue;
      const path = join(directory, entry.name);
      if (entry.name.startsWith("@")) {
        for (const scoped of await readdir(path)) await scanPackage(join(path, scoped));
      } else await scanPackage(path);
    }
  }
  async function scanPackage(directory) {
    let pkg;
    try {
      pkg = JSON.parse(await readFile(join(directory, "package.json"), "utf8"));
    } catch (error) {
      if (["ENOENT", "ENOTDIR"].includes(error.code)) return;
      throw error;
    }
    if (pkg.name && pkg.version) installed.set(`${pkg.name}@${pkg.version}`, { pkg, directory });
    await scanNodeModules(join(directory, "node_modules"));
  }
  for (const project of projects) await scanNodeModules(join(project.path, "node_modules"));
  await scanNodeModules(join(root, "node_modules"));
  await scanNodeModules(join(root, "apps/zcode-cli/node_modules"));
  return installed;
}

export function missingProductionPackages(required, installed) {
  const missing = [...required].filter(([key]) => !installed.has(key)).map(([, item]) => item);
  for (const item of missing) {
    if (!unsupportedCanvas.has(item.name))
      throw new Error(`Missing installed dependency: ${item.name}@${item.version}`);
  }
  return missing;
}

export async function collectNpmNotices(root, overrides) {
  root = await realpath(root);
  const { required, projects } = await readWorkspaceProductionGraph(root);
  // 修复：标识门禁和声明生成必须扫描同一安装集合，避免嵌套版本只进声明、不进门禁。
  const installed = await scanInstalledPackages(root, projects);
  const packages = [];
  const missing = [];
  const notInstalled = missingProductionPackages(required, installed);
  for (const [key, item] of [...required].sort(([a], [b]) => a.localeCompare(b, "en"))) {
    const installedPackage = installed.get(key);
    if (!installedPackage) {
      continue;
    }
    const { pkg, directory } = installedPackage;
    const notices = await readPackageNotices(directory);
    const override = overrides.find((record) => record.package === key);
    if (override?.file) {
      const bytes = await readFile(join(root, override.file));
      if (hashBytes(bytes) !== override.sha256) throw new Error(`Changed upstream notice: ${key}`);
      notices.push({ member: override.source, bytes });
    }
    // README 中仅有 MIT 等标签不能冒充完整许可文件；这种包仍需要版本固定的补充材料。
    if (
      !notices.some(({ member }) => !member.endsWith(" (license section)")) &&
      !override?.acceptedMissingNotice
    )
      missing.push(key);
    packages.push({
      ...item,
      license:
        pkg.license ??
        pkg.licenses?.map((item) => (typeof item === "string" ? item : item.type)).join(" OR ") ??
        override?.license ??
        "(not declared)",
      repository: typeof pkg.repository === "string" ? pkg.repository : pkg.repository?.url,
      ...(override?.acceptedMissingNotice
        ? { acceptedMissingNotice: override.acceptedMissingNotice }
        : {}),
      notices,
    });
  }
  if (missing.length) throw new Error(`Missing complete upstream notices:\n${missing.join("\n")}`);
  return {
    packages,
    notInstalled,
    workspaceManifests: projects.map((project) =>
      relative(root, join(project.path, "package.json")).replaceAll("\\", "/"),
    ),
  };
}
