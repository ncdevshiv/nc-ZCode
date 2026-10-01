import { existsSync, promises as fs, statSync } from "node:fs";
import path from "node:path";
import ts from "typescript";
import { parse as parseYaml } from "yaml";

const SOURCE_EXTENSIONS = [".ts", ".tsx", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs"];

export { SOURCE_EXTENSIONS };

export function posix(value) {
  return value.split(path.sep).join("/");
}

function isSourceFile(file) {
  return SOURCE_EXTENSIONS.some((extension) => file.endsWith(extension));
}

async function walk(root, files) {
  let entries;
  try {
    entries = await fs.readdir(root, { withFileTypes: true });
  } catch {
    return;
  }
  for (const entry of entries) {
    if (["node_modules", "dist", "out", "coverage", ".git"].includes(entry.name)) continue;
    const target = path.join(root, entry.name);
    if (entry.isDirectory()) await walk(target, files);
    else if (entry.isFile() && isSourceFile(target)) files.push(target);
  }
}

function validatePolicy(raw, cwd) {
  if (!raw || raw.version !== 1 || !Array.isArray(raw.modules)) {
    throw new Error("architecture-policy.yaml 必须包含 version: 1 和 modules 数组");
  }
  const ids = new Set();
  const modules = raw.modules.map((module) => {
    if (!module?.id || ids.has(module.id))
      throw new Error(`模块 id 重复或为空: ${module?.id ?? ""}`);
    ids.add(module.id);
    if (!Array.isArray(module.roots) || module.roots.length === 0) {
      throw new Error(`模块 ${module.id} 必须声明 roots`);
    }
    return {
      id: module.id,
      roots: module.roots.map((root) => path.resolve(cwd, root)),
      managed: module.managed === true,
      requires: [...new Set(module.requires ?? [])],
      publicEntrypoints: (module.publicEntrypoints ?? []).map((entry) => posix(entry)),
      layers: module.layers ?? {},
      layerOrder: module.layerOrder ?? Object.keys(module.layers ?? {}),
      owner: module.owner ?? null,
    };
  });
  const moduleIds = new Set(modules.map((module) => module.id));
  for (const module of modules) {
    for (const dependency of module.requires) {
      if (!moduleIds.has(dependency))
        throw new Error(`模块 ${module.id} 依赖未知模块 ${dependency}`);
    }
  }
  const global = {
    maxFileLines: raw.global?.maxFileLines ?? 400,
    maxContractLines: raw.global?.maxContractLines ?? 300,
    forbidCycles: raw.global?.forbidCycles !== false,
    forbidDeepImports: raw.global?.forbidDeepImports !== false,
    managedOnly: raw.global?.managedOnly !== false,
    maxPublicMethods: raw.global?.maxPublicMethods ?? 12,
  };
  const exceptions = (raw.exceptions ?? []).map((exception) => ({
    ...exception,
    paths: exception.paths ?? [],
  }));
  return {
    version: 1,
    modules,
    global,
    exceptions,
    languageBoundary: validateLanguageBoundary(raw.languageBoundary, cwd),
  };
}

/**
 * 规范化 languageBoundary 段：把 roots/files/patterns 三种写法压平成同一种
 * 「可比较的字面量前缀 + 匹配器」，让优先级比较（最具体者胜）不需要分支。
 */
function validateLanguageBoundary(raw, cwd) {
  if (raw === undefined || raw === null) return null;
  if (typeof raw.id !== "string" || raw.id.length === 0)
    throw new Error("languageBoundary.id 必须是非空字符串");
  const extensions = (raw.extensions ?? []).map((extension) =>
    extension.startsWith(".") ? extension : `.${extension}`,
  );
  if (extensions.length === 0) throw new Error("languageBoundary.extensions 不能为空");

  const ids = new Set();
  const allowlist = (raw.allowlist ?? []).map((entry) => {
    if (!entry?.id || ids.has(entry.id))
      throw new Error(`languageBoundary.allowlist id 重复或为空: ${entry?.id ?? ""}`);
    ids.add(entry.id);
    const kind = entry.kind === "dev-tooling" ? "dev-tooling" : "frontend";
    const items = [
      ...(entry.roots ?? []).map((root) => ({
        type: "root",
        value: posix(path.relative(cwd, path.resolve(cwd, root))).replace(/^\.$/, ""),
      })),
      ...(entry.files ?? []).map((file) => ({ type: "file", value: posix(file) })),
      ...(entry.patterns ?? []).map((pattern) => ({ type: "pattern", value: posix(pattern) })),
    ];
    if (items.length === 0)
      throw new Error(`languageBoundary.allowlist 条目 ${entry.id} 必须声明 roots/files/patterns`);
    return {
      id: entry.id,
      kind,
      items: items.map((item) => ({ ...item, literal: literalPrefix(item.value) })),
    };
  });

  const escapeHatches = (raw.escapeHatches ?? []).map((hatch) => {
    if (!hatch?.id) throw new Error("languageBoundary.escapeHatches 每条都需要 id");
    if (!hatch.reason)
      throw new Error(`逃生舱 ${hatch.id} 必须写 reason：为什么这个非前端 TS 合法`);
    if (!hatch.approvedBy) throw new Error(`逃生舱 ${hatch.id} 必须写 approvedBy：谁批准的`);
    if (!hatch.expires) throw new Error(`逃生舱 ${hatch.id} 必须写 expires：逃生舱必须自己到期`);
    const patterns = hatch.paths ?? [];
    // MINOR 2：hatch 的 paths 只支持「精确文件」与「glob 模式」两种（本轮不引入 roots，
    // 那需要产品决策）。含通配符的按 pattern 走；不含通配符的按**精确文件**走——所以
    // 作者写 paths: ["packages/svc"]（一个目录）会静默变成「只豁免这个叫 packages/svc
    // 的文件」，目录里的真实文件全部照报新增。静默错配比报错更贵，所以在加载 policy 时
    // 就把它顶回去，并告诉作者该怎么写。
    for (const pattern of patterns) {
      if (isGlobPattern(pattern)) continue;
      const absolute = path.resolve(cwd, pattern);
      if (!existsSync(absolute) || !statSync(absolute).isDirectory()) continue;
      const suggestion = pattern.replace(/\/$/, "") + "/**/*.ts";
      throw new Error(
        `逃生舱 ${hatch.id} 的 paths[${pattern}] 是一个目录，但无通配符的路径按**精确文件**匹配。` +
          `请改成 glob：${suggestion}`,
      );
    }
    if (patterns.length === 0) throw new Error(`逃生舱 ${hatch.id} 必须声明 paths`);
    return {
      id: hatch.id,
      reason: hatch.reason,
      approvedBy: hatch.approvedBy,
      expires: hatch.expires,
      items: patterns.map((pattern) => ({
        type: isGlobPattern(pattern) ? "pattern" : "file",
        value: posix(pattern),
        literal: literalPrefix(pattern),
      })),
    };
  });

  return {
    id: raw.id,
    extensions,
    allowlist,
    forbidDevToolingImports: raw.forbidDevToolingImports !== false,
    escapeHatches,
  };
}

/**
 * 唯一的「这是不是一个 glob」判据，被逃生舱的目录守卫与 items 构造共用。
 *
 * 只认 `*`：`matchSegment` 只实现了 `*`（`**` 由 matchPattern 单独处理），`?`
 * 从来不是通配符。守卫、items 与 literalPrefix 三处必须用同一个判据——守卫一旦
 * 单独多认 `?`，就会出现「守卫以为它是 glob、items 却当精确文件」的静默错配：
 * 目录不被顶回去、舱也不生效，且没有任何报错。
 */
function isGlobPattern(value) {
  return value.includes("*");
}

/** 通配符之前的字面量长度——优先级比较用（spec §2.2「最具体者胜」）。 */
function literalPrefix(pattern) {
  // 只认 `*`，理由同 isGlobPattern：`?` 在 matchSegment 里没有实现。
  const wildcard = pattern.search(/\*/);
  const literal = wildcard === -1 ? pattern : pattern.slice(0, wildcard);
  // 目录前缀的末尾斜杠不携带额外区分度，剥掉以免 "a/" 压过 "a/b"。
  return literal.replace(/[/.]+$/, "").length;
}

/** 支持 **（跨任意层，可匹配零层）与 *（单层内任意字符）的最小 glob，刻意不引新依赖。 */
export function matchPattern(pattern, file) {
  const parts = pattern.split("/");
  const segments = file.split("/");
  const walk = (patternIndex, segmentIndex) => {
    if (patternIndex === parts.length) return segmentIndex === segments.length;
    const part = parts[patternIndex];
    if (part === "**") {
      // ** 匹配零层或多层；吃掉最后一层后必须整体消费掉。
      if (patternIndex === parts.length - 1) return true;
      for (let skip = segmentIndex; skip <= segments.length; skip += 1) {
        if (walk(patternIndex + 1, skip)) return true;
      }
      return false;
    }
    if (segmentIndex >= segments.length) return false;
    if (!matchSegment(part, segments[segmentIndex])) return false;
    return walk(patternIndex + 1, segmentIndex + 1);
  };
  return walk(0, 0);
}

function matchSegment(pattern, segment) {
  if (pattern === "*") return true;
  if (!pattern.includes("*")) return pattern === segment;
  const source = pattern
    .split("*")
    .map((part) => part.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"))
    .join("[^/]*");
  return new RegExp(`^${source}$`).test(segment);
}

function matchesItem(item, file) {
  if (item.type === "file") return item.value === file;
  if (item.type === "root") return file === item.value || file.startsWith(`${item.value}/`);
  return matchPattern(item.value, file);
}

/**
 * spec §2.2 优先级：字面前缀最长者胜；同长度且同时命中时若含已过期逃生舱，
 * 报 language-boundary-ambiguous（策略歧义必须改 policy，门禁不猜）。
 */
export function classifyPath(file, languageBoundary, today) {
  const matches = [];
  for (const entry of languageBoundary.allowlist) {
    for (const item of entry.items) {
      if (matchesItem(item, file))
        matches.push({ source: "allowlist", entry, item, literal: item.literal, expired: false });
    }
  }
  // 过期逃生舱**无条件**记进 expiredHatches：不看谁赢下优先级。
  // 理由（spec 2.2 rule 1 / 5）：到期必须始终可见，否则"过期宽逃生舱 + 更宽 allowlist"
  // 会永久静默地让过期口子合法化——而那恰好是最该被看见的情况。
  // 只在"赢下优先级才报"时，一个比 allowlist 宽的 ** 逃生舱永远不会被报出来。
  const expiredHatches = [];
  for (const hatch of languageBoundary.escapeHatches) {
    const expired = Boolean(hatch.expires && hatch.expires < today);
    for (const item of hatch.items) {
      if (!matchesItem(item, file)) continue;
      matches.push({ source: "hatch", entry: hatch, item, literal: item.literal, expired });
      if (expired && !expiredHatches.includes(hatch)) expiredHatches.push(hatch);
    }
  }
  const base = {
    allowlist: null,
    hatch: null,
    ambiguous: false,
    expiredHatches,
    conflictingHatch: null,
  };
  if (matches.length === 0) return { ...base, verdict: "debt" };
  matches.sort((a, b) => b.literal - a.literal);
  const winner = matches[0];
  // 真正的歧义：同优先级（字面前缀等长）下两条都命中且来源不同，门禁不猜，必须改 policy。
  const tie = matches.filter((match) => match.literal === winner.literal);
  const ambiguous = tie.length > 1 && new Set(tie.map((match) => match.source)).size > 1;
  // 平局里真正冲突的那条逃生舱：诊断要指名道姓，而不是只说「策略有歧义」。
  const conflictingHatch = tie.find((match) => match.source === "hatch")?.entry ?? null;
  if (winner.source === "allowlist")
    return {
      ...base,
      verdict: "allowlisted",
      allowlist: winner.entry,
      ambiguous,
      conflictingHatch,
    };
  // 过期逃生舱**不豁免**该路径：到期之后再按"合法"处理，等于"过期"让棘轮变松——
  // 路径落回债分支，基线之外就是新增违例。allowlist 命中时路径仍合法（spec 2.2
  // rule 1：allowlist 压过逃生舱），但到期照样在 expiredHatches 里报出来。
  if (winner.expired) return { ...base, verdict: "debt", ambiguous, conflictingHatch };
  return { ...base, verdict: "hatched", hatch: winner.entry, ambiguous, conflictingHatch };
}

export function layerForFile(file, module) {
  const matches = module.roots
    .flatMap((root) =>
      Object.entries(module.layers).map(([name, directory]) => ({
        name,
        directory: path.resolve(root, directory),
      })),
    )
    .filter(({ directory }) => file === directory || file.startsWith(`${directory}${path.sep}`))
    .sort((a, b) => b.directory.length - a.directory.length);
  return matches[0]?.name ?? null;
}

export function publicEntrypointMatches(target, targetModule, cwd) {
  return targetModule.publicEntrypoints.some((entry) => {
    const candidates = [
      path.resolve(cwd, entry),
      ...targetModule.roots.map((root) => path.resolve(root, entry)),
    ];
    return candidates.includes(target);
  });
}

export function manifestRequires(source) {
  const match = source.match(/requires\s*:\s*\[([^\]]*)\]/s);
  if (!match) return null;
  return [...match[1].matchAll(/["']([^"']+)["']/g)].map((item) => item[1]);
}

export function countPublicMethods(source) {
  const sourceFile = ts.createSourceFile("contract.ts", source, ts.ScriptTarget.Latest, true);
  let count = 0;
  function visit(node) {
    if (ts.isInterfaceDeclaration(node) || ts.isClassDeclaration(node)) {
      for (const member of node.members) {
        if (
          ts.isMethodSignature(member) ||
          ts.isMethodDeclaration(member) ||
          ts.isCallSignatureDeclaration(member)
        )
          count += 1;
      }
    }
    ts.forEachChild(node, visit);
  }
  visit(sourceFile);
  return count;
}

export async function loadPolicy(cwd = process.cwd()) {
  const filename = path.join(cwd, "architecture-policy.yaml");
  const content = await fs.readFile(filename, "utf8");
  return validatePolicy(parseYaml(content), cwd);
}

export function moduleForFile(file, policy) {
  const candidates = policy.modules
    .filter((module) =>
      module.roots.some((root) => file === root || file.startsWith(`${root}${path.sep}`)),
    )
    .sort(
      (a, b) =>
        Math.max(...b.roots.map((root) => root.length)) -
        Math.max(...a.roots.map((root) => root.length)),
    );
  return candidates[0] ?? null;
}

export async function discoverFiles(policy) {
  const files = [];
  for (const module of policy.modules) {
    for (const root of module.roots) await walk(root, files);
  }
  return [...new Set(files)].sort();
}

export function resolveImport(from, specifier, knownFiles) {
  if (!specifier.startsWith(".")) return null;
  let base = path.resolve(path.dirname(from), specifier);
  if (SOURCE_EXTENSIONS.some((extension) => base.endsWith(extension))) {
    base = base.slice(0, base.lastIndexOf("."));
  }
  const candidates = [];
  for (const extension of SOURCE_EXTENSIONS) candidates.push(`${base}${extension}`);
  for (const extension of SOURCE_EXTENSIONS) candidates.push(path.join(base, `index${extension}`));
  return candidates.find((candidate) => knownFiles.has(candidate)) ?? null;
}

export function importsOf(file, source) {
  return importSitesOf(file, source).map((site) => site.specifier);
}

/** 与 importsOf 同源，额外给出声明所在行号——ratchet 的违例必须能指到 file:line。 */
export function importSitesOf(file, source) {
  const sourceFile = ts.createSourceFile(file, source, ts.ScriptTarget.Latest, true);
  const imports = [];
  function visit(node) {
    if (
      (ts.isImportDeclaration(node) || ts.isExportDeclaration(node)) &&
      node.moduleSpecifier &&
      ts.isStringLiteral(node.moduleSpecifier)
    ) {
      imports.push({
        specifier: node.moduleSpecifier.text,
        line: sourceFile.getLineAndCharacterOfPosition(node.getStart(sourceFile)).line + 1,
      });
    }
    if (ts.isImportEqualsDeclaration(node) && ts.isExternalModuleReference(node.moduleReference)) {
      if (ts.isStringLiteral(node.moduleReference.expression))
        imports.push({
          specifier: node.moduleReference.expression.text,
          line: sourceFile.getLineAndCharacterOfPosition(node.getStart(sourceFile)).line + 1,
        });
    }
    ts.forEachChild(node, visit);
  }
  visit(sourceFile);
  return imports;
}

/** 便宜的前置过滤：只有可能指向相对路径依赖的源文件才值得做完整 AST 解析。 */
export function hasRelativeImport(source) {
  return /(?:from|import|require)\s*\(?\s*["']\.[^"']*["']/.test(source);
}

export function isException(policy, rule, file, cwd) {
  const relative = posix(path.relative(cwd, file));
  return policy.exceptions.some(
    (exception) =>
      exception.rule === rule &&
      exception.paths.some(
        (pattern) => relative === pattern || relative.startsWith(pattern.replace(/\*\*$/, "")),
      ),
  );
}
