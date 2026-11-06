import { createHash } from "node:crypto";
import { readFile, readdir, realpath } from "node:fs/promises";
import { join, relative, resolve } from "node:path";

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
      `Installed production graph differs from bun.lock. Run bun install --frozen-lockfile.\nMissing: ${missing.map(([key]) => key).join(", ")}\nStale: ${stale.join(", ")}`,
    );
  }
  return locked;
}

function parseBunLockfile(text) {
  // bun.lock is JSONC (trailing commas). Bun ships a JSONC parser; Node only
  // needs the trailing commas removed.
  if (typeof Bun !== "undefined" && Bun.JSONC?.parse) return Bun.JSONC.parse(text);
  return JSON.parse(text.replace(/,(\s*[}\]])/gu, "$1"));
}

/**
 * Resolve the workspace production dependency graph from Bun's lockfile.
 *
 * Bun lockfile v2 records every workspace manifest and every resolved package
 * (keyed by package name for the hoisted install), so the graph can be read
 * without spawning a package manager. This replaces the previous `pnpm ls`
 * walk, which accumulated file handles across the workspace on Windows.
 */
export async function readWorkspaceProductionGraph(root) {
  root = await realpath(root);
  const lockfile = parseBunLockfile(await readFile(join(root, "bun.lock"), "utf8"));
  const workspaces = Object.entries(lockfile.workspaces ?? {});
  const workspaceNames = new Set(workspaces.map(([, item]) => item.name).filter(Boolean));
  const packages = lockfile.packages ?? {};
  const projects = workspaces
    .filter(([, item]) => typeof item.name === "string")
    .map(([path, item]) => ({
      dependencies: item.dependencies ?? {},
      name: item.name,
      optionalDependencies: item.optionalDependencies ?? {},
      path: resolve(root, path),
    }));
  const required = new Map();
  const resolveEntry = (name, parentKey) => {
    // Bun nests a second version under `<parent>/<name>`; fall back to the
    // hoisted root entry when no nested copy exists.
    if (parentKey && packages[`${parentKey}/${name}`]) {
      return { key: `${parentKey}/${name}`, entry: packages[`${parentKey}/${name}`] };
    }
    return packages[name] ? { key: name, entry: packages[name] } : undefined;
  };
  const visit = (name, parentKey) => {
    // Workspace packages are linked, not installed from the registry.
    if (workspaceNames.has(name) || name.startsWith("@zcode/")) return;
    const resolvedEntry = resolveEntry(name, parentKey);
    if (!resolvedEntry) return; // Platform-specific optional dependency for another OS.
    const resolution = resolvedEntry.entry[0];
    const separator = resolution.lastIndexOf("@");
    const packageName = resolution.slice(0, separator);
    const version = resolution.slice(separator + 1);
    if (required.has(`${packageName}@${version}`)) return;
    required.set(`${packageName}@${version}`, { name: packageName, version });
    const meta = resolvedEntry.entry[2] ?? {};
    for (const dependency of Object.keys(meta.dependencies ?? {})) {
      visit(dependency, resolvedEntry.key);
    }
    for (const dependency of Object.keys(meta.optionalDependencies ?? {})) {
      visit(dependency, resolvedEntry.key);
    }
  };
  for (const project of projects) {
    for (const name of [
      ...Object.keys(project.dependencies),
      ...Object.keys(project.optionalDependencies),
    ]) {
      visit(name);
    }
  }
  return { required, projects };
}

export async function scanInstalledPackages(root, projects) {
  // Bun's hoisted layout may still expose a package directory through several
  // links; match on the real install directory and exact version.
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
