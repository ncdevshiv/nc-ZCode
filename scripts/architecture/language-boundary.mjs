// 语言边界棘轮：TypeScript 只存在于前端。规范来源 specs/typescript-frontend-only.md。
//
// 独立成模块的原因：这是**仓库级**规则（不是某个 module 的规则）——文件集合来自 git 枚举
// 而不是 module roots，基线也独立于 .architecture-baseline.json。塞在 index.mjs 会让那个
// 文件同时承担两种规则的文件发现与基线职责，并把它推过 max-lines 上限。
import { promises as fs } from "node:fs";
import path from "node:path";

import { gitFileNames } from "./git-file-names.mjs";
import { violation } from "./violation.mjs";
import {
  classifyPath,
  hasRelativeImport,
  importSitesOf,
  loadPolicy,
  posix,
  resolveImport,
  SOURCE_EXTENSIONS,
} from "./policy.mjs";

// ── 语言边界棘轮（specs/typescript-frontend-only.md）────────────────────────
// 基线是「今天的迁移债」快照。此后只有新增/扩大才失败，删除与缩小自动减债。
// 它刻意不进入 result.violations：模块规则的 baseline:update 绝不能顺手把
// 新的语言债写进 .architecture-baseline.json 洗白。报告与退出码仍是同一个门禁。

const TS_BASELINE_FILE = ".architecture-ts-baseline.json";

async function readTsBaseline(cwd) {
  try {
    return JSON.parse(await fs.readFile(path.join(cwd, TS_BASELINE_FILE), "utf8"));
  } catch (error) {
    if (error.code === "ENOENT")
      throw Object.assign(
        new Error(
          `缺少 ${TS_BASELINE_FILE}：语言边界没有基线就无法区分「存量债」与「新增债」。` +
            `先运行 pnpm architecture:ts-baseline:update 生成。`,
        ),
        { code: "ENOENT" },
      );
    throw error;
  }
}

/** 仓库级 TS 文件集合：git 的索引 + 未跟踪未忽略，再剔掉工作区已删除的。 */
async function repositoryFiles(cwd) {
  const names = await gitFileNames(cwd, [
    "ls-files",
    "-z",
    "--cached",
    "--others",
    "--exclude-standard",
  ]);
  return names.filter(Boolean);
}

async function existingSourceFiles(cwd, files) {
  const found = [];
  for (const file of files) {
    const absolute = path.resolve(cwd, file);
    try {
      if ((await fs.stat(absolute)).isFile()) found.push(absolute);
    } catch {
      // 工作区已删除 = 减债，不参与判定。
    }
  }
  return found;
}

/**
 * @param baseline 显式传入基线。ts-baseline:update 传空对象做引导：没有基线时
 * 所有非 allowlist 的 TS 都算「新增」，这正是首次生成的语义。
 */
export async function checkLanguageBoundary({ cwd = process.cwd(), policy, baseline = undefined }) {
  const boundary = policy.languageBoundary;
  if (!boundary) return null;
  const today = new Date().toISOString().slice(0, 10);
  const tsBaseline = baseline ?? (await readTsBaseline(cwd));
  const baselineDebt = tsBaseline.debt ?? {};
  const baselineEdges = tsBaseline.devToolingEdges ?? {};
  const known = await repositoryFiles(cwd);
  // 判定对象是 TS 家族；但 dev-tooling 依赖规则要解析到 .mjs/.js 的开发工具文件，
  // 所以已知文件集合必须覆盖全部源码扩展名，否则 `import "../../scripts/x.mjs"` 会绕过。
  const existing = await existingSourceFiles(
    cwd,
    known.filter((file) => SOURCE_EXTENSIONS.some((extension) => file.endsWith(extension))),
  );
  const existingSet = new Set(existing.map((file) => path.resolve(file)));

  const violations = [];
  const debt = new Map();
  const debtFiles = [];
  const toolingFiles = new Set();
  let allowedFiles = 0;
  let parsedImports = 0;

  // N1：过期逃生舱按**舱**去重，不按文件。舱是策略级事实（一条 policy 记录），
  // 每个命中的文件都报一次会把唯一可执行的那一行埋掉：一条 `**` 逃生舱在本仓库
  // 会命中几千个文件，输出全是同一句话。
  const reportedHatches = new Set();
  // MAJOR 2：被**活逃生舱**放行的 TS 既不进 `debt`、也不受基线比较，等于从棘轮里
  // 整段消失。只报 debt 会让这个洞在标题数字里看不见（审计实测：一条活舱就能让
  // 债务显示少 9000 行而门禁仍 OK）。所以必须把放行量与债务量**并排**报出来。
  const hatchedFiles = [];
  let hatchedLines = 0;

  for (const file of existing) {
    const relative = posix(path.relative(cwd, file));
    const verdict = classifyPath(relative, boundary, today);
    // 过期逃生舱：复用门禁既有的 expired-exception 规则，与 policy.exceptions 的到期
    // 报告走同一条路径（不新造规则名，过期就是过期）。
    for (const hatch of verdict.expiredHatches ?? []) {
      if (reportedHatches.has(hatch.id)) continue;
      reportedHatches.add(hatch.id);
      violations.push(
        violation({
          rule: "expired-exception",
          file: path.join(cwd, "architecture-policy.yaml"),
          detail: hatch.id,
          message: `语言边界逃生舱 ${hatch.id} 已于 ${hatch.expires} 过期（UTC）`,
          global: true,
        }),
      );
    }
    // MINOR 1：只在**平局里真的含过期逃生舱**时才报（spec 2.2 rule 4 的原话），
    // 并指名那条真正冲突的舱。旧写法在未过期的平局上也贴「已过期」，detail 还填的是
    // allowlist 的 id——既假又指错地方。
    if (verdict.ambiguous && verdict.expiredHatches.length > 0)
      violations.push(
        violation({
          rule: "language-boundary-ambiguous",
          file: path.join(cwd, "architecture-policy.yaml"),
          detail: `tie:${verdict.conflictingHatch?.id ?? "?"}+${verdict.allowlist?.id ?? "?"}`,
          message: `路径 ${relative} 同时命中同优先级的 allowlist 与已过期逃生舱 ${verdict.conflictingHatch?.id ?? "?"}，策略有歧义，请改 architecture-policy.yaml`,
          global: true,
        }),
      );
    if (verdict.allowlist?.kind === "dev-tooling") toolingFiles.add(path.resolve(file));
    if (!boundary.extensions.some((extension) => file.endsWith(extension))) continue;
    if (verdict.verdict !== "debt") {
      allowedFiles += 1;
      // 放行量要能被看见：allowlist 的量是恒定基线（可预期），逃生舱的量不是——
      // 它由人写 policy 决定，所以必须单独成行，否则与 allowlist 混在一起就看不出异常。
      if (verdict.hatch) {
        // 放行量也要按行数报：只有文件数看不出「放行了 1 个 9 万行的文件」。
        // 行数口径与债务侧一致（split(/\r?\n/).length），保证两侧可比。
        const source = await fs.readFile(file, "utf8");
        hatchedFiles.push(relative);
        hatchedLines += source.split(/\r?\n/).length;
      }
      continue;
    }
    const source = await fs.readFile(file, "utf8");
    const lines = source.split(/\r?\n/).length;
    debt.set(relative, lines);
    debtFiles.push(file);
    const allowed = baselineDebt[relative] ?? 0;
    if (lines > allowed) {
      const kind = allowed === 0 ? "新增" : "扩大";
      violations.push(
        violation({
          rule: "ts-frontend-only",
          file,
          detail: String(lines),
          message:
            `非前端 TypeScript ${kind}：${relative} 现在 ${lines} 行，基线 ${allowed} 行。` +
            `修法：① 不要加这段 TS；② 确实是前端就落到 allowlist 内；` +
            `③ 确实需要（例如与冻结 fixture 对比的测试）就在 languageBoundary.escapeHatches ` +
            `加一条带 reason/approvedBy/expires 的条目。禁止把后端逻辑搬进 packages/ui 来压行数。`,
          global: true,
        }),
      );
    }
  }

  // dev-tooling 停车位规则：非 allowlist 的 TS 不得依赖 dev-tooling 路径，
  // 否则允许清单会退化成后端 TS 的停车场。先用正则筛，只对可能有相对依赖的文件做 AST 解析。
  const edges = new Map();
  if (boundary.forbidDevToolingImports && toolingFiles.size > 0) {
    for (const file of debtFiles) {
      const source = await fs.readFile(file, "utf8");
      if (!hasRelativeImport(source)) continue;
      parsedImports += 1;
      const relative = posix(path.relative(cwd, file));
      for (const site of importSitesOf(file, source)) {
        const target = resolveImport(file, site.specifier, existingSet);
        if (!target || !toolingFiles.has(target)) continue;
        const targetRelative = posix(path.relative(cwd, target));
        const key = `${relative}|${targetRelative}`;
        const sites = edges.get(key) ?? [];
        sites.push({ line: site.line, specifier: site.specifier });
        edges.set(key, sites);
      }
    }
    // 边也是棘轮：今天已存在的边是继承债，只有新增边或同边条数超过基线才报（spec 3.4.1）。
    for (const [key, sites] of edges) {
      const [importer, targetRelative] = key.split("|");
      const allowed = baselineEdges[key] ?? 0;
      if (sites.length <= allowed) continue;
      const kind = allowed === 0 ? "新增" : "扩大";
      // 每条超额边都要指到具体 file:line，不能只说"这条边超了"。
      for (const site of sites.slice(allowed)) {
        violations.push(
          violation({
            rule: "dev-tooling-import",
            file: path.join(cwd, importer),
            detail: `${importer}:${site.line} -> ${targetRelative}`,
            message:
              `非前端 TS ${kind}依赖开发工具代码：${importer}:${site.line} import ${site.specifier} -> ${targetRelative}` +
              `（基线 ${allowed} 条，现在 ${sites.length} 条）。` +
              `修法：反转依赖方向（开发工具依赖业务，而不是业务依赖开发工具），或把共用工具移出 scripts/ 到中立模块。`,
            global: true,
          }),
        );
      }
    }
  }

  return {
    rule: boundary.id,
    violations,
    debt,
    edges,
    stats: {
      files: debt.size,
      lines: [...debt.values()].reduce((total, value) => total + value, 0),
      allowedFiles,
      hatchedFiles: hatchedFiles.length,
      hatchedLines,
      baselineFiles: Object.keys(baselineDebt).length,
      baselineLines: Object.values(baselineDebt).reduce((total, value) => total + value, 0),
      edges: [...edges.values()].reduce((total, sites) => total + sites.length, 0),
      baselineEdges: Object.values(baselineEdges).reduce((total, value) => total + value, 0),
      parsedImports,
    },
  };
}

/**
 * 重新生成棘轮基线。默认只允许收缩：扩大必须显式 --accept-growth 并留下
 * reason / approvedBy，且事件被追加进 acceptedGrowth（覆盖式洗白不留痕）。
 */
export async function updateTsBaseline({ cwd = process.cwd(), acceptGrowth = null } = {}) {
  const policy = await loadPolicy(cwd);
  // 引导语义：空基线 = 所有非 allowlist 的 TS 都是「新增」，生成器据此冻结今天的债务。
  const current = await checkLanguageBoundary({ cwd, policy, baseline: { debt: {} } });
  const debt = Object.fromEntries(
    [...current.debt.entries()].sort(([a], [b]) => a.localeCompare(b)),
  );
  const edgeDebt = Object.fromEntries(
    [...current.edges.entries()]
      .map(([key, sites]) => [key, sites.length])
      .sort(([a], [b]) => a.localeCompare(b)),
  );
  const previous = await readTsBaseline(cwd).catch((error) => {
    // 首次生成（仓库还没有基线文件）是引导，不是「扩大」：没有它就没有任何基线。
    if (error.code === "ENOENT") return null;
    throw error;
  });
  const previousDebt = previous?.debt ?? {};
  const previousEdges = previous?.devToolingEdges ?? {};
  const added = Object.keys(debt).filter((file) => !(file in previousDebt));
  const removed = Object.keys(previousDebt).filter((file) => !(file in debt));
  const grown = Object.keys(debt).filter(
    (file) => file in previousDebt && debt[file] > previousDebt[file],
  );
  const shrunk = Object.keys(debt).filter(
    (file) => file in previousDebt && debt[file] < previousDebt[file],
  );
  const previousLines = Object.values(previousDebt).reduce((total, value) => total + value, 0);
  const lines = current.stats.lines;
  const previousEdgeCount = Object.values(previousEdges).reduce((total, value) => total + value, 0);
  const edgeCount = current.stats.edges;
  const newEdges = Object.keys(edgeDebt).filter((key) => !(key in previousEdges));
  const grownEdges = Object.keys(edgeDebt).filter(
    (key) => key in previousEdges && edgeDebt[key] > previousEdges[key],
  );
  const removedEdges = Object.keys(previousEdges).filter((key) => !(key in edgeDebt));
  const summary = {
    bootstrapped: previous === null,
    added: added.length,
    removed: removed.length,
    grown: grown.length,
    shrunk: shrunk.length,
    previousLines,
    lines,
    delta: lines - previousLines,
    edgeCount,
    previousEdgeCount,
    edgeDelta: edgeCount - previousEdgeCount,
    newEdges: newEdges.length,
    grownEdges: grownEdges.length,
    removedEdges: removedEdges.length,
    // 活逃生舱放行的量必须跟着这条命令一起打印：被舱藏起来的文件会从基线里
    // "消失"，如果不显式报出舱的覆盖面，`Δ` 为负看起来就像诚实的减债。
    hatchedFiles: current.stats.hatchedFiles,
    hatchedLines: current.stats.hatchedLines,
  };
  // 两条轴都要只减不增：行数是"写出来的债"，边数是"依赖方向的债"。
  const axes = [];
  if (!summary.bootstrapped && summary.delta > 0) axes.push("lines");
  if (!summary.bootstrapped && summary.edgeDelta > 0) axes.push("devToolingEdges");
  summary.grownAxes = axes;
  if (!summary.bootstrapped && axes.length > 0 && !acceptGrowth)
    throw new Error(
      `拒绝扩大 TypeScript 债务基线：` +
        `${axes.map((axis) => (axis === "lines" ? `行数 ${lines} > ${previousLines}（+${summary.delta}，新增 ${summary.added} 个文件）` : `dev-tooling 依赖边 ${edgeCount} > ${previousEdgeCount}（+${summary.edgeDelta}，新增边 ${summary.newEdges}，扩大边 ${summary.grownEdges}）`)).join("；")}。` +
        `只有仓库 owner 能批准；确认要扩大就显式传 --accept-growth --reason "<为什么>" --approved-by "<谁>"。`,
    );
  const acceptedGrowth = [...(previous?.acceptedGrowth ?? [])];
  if (!summary.bootstrapped && axes.length > 0)
    acceptedGrowth.push({
      at: new Date().toISOString().slice(0, 10),
      reason: acceptGrowth.reason,
      approvedBy: acceptGrowth.approvedBy,
      axes,
      lineDelta: summary.delta,
      addedFiles: added.length,
      grownFiles: grown.length,
      edgeDelta: summary.edgeDelta,
      newEdges: newEdges.length,
      grownEdges: grownEdges.length,
    });
  const today = new Date().toISOString().slice(0, 10);
  const payload = {
    version: 1,
    rule: current.rule,
    generatedAt: today,
    generator: "pnpm architecture:ts-baseline:update",
    // 引导日期冻结在第一次生成，之后任何再生成都不会把它抹掉——读者一眼能看出
    // 这份基线从哪天起就该只减不增，以及之后批准过几次扩大。
    bootstrappedAt: previous?.bootstrappedAt ?? today,
    totals: { files: Object.keys(debt).length, lines, devToolingEdges: edgeCount },
    acceptedGrowth,
    debt,
    devToolingEdges: edgeDebt,
  };
  // 这个文件是**生成物**：写出的字节就是 `JSON.stringify` 的结果，不做手工美化。
  // 曾经把它交给 oxfmt，结果是"提交的基线"与"生成器的输出"对不上
  // （acceptedGrowth.axes 被折成一行），谁先跑哪个命令就得到不同文件。
  // 现在反过来：把 `.architecture-ts-baseline.json` 写进 `.oxfmtrc.json` 的
  // ignorePatterns，让生成器与入库产物在字节上恒等。
  await fs.writeFile(path.join(cwd, TS_BASELINE_FILE), `${JSON.stringify(payload, null, 2)}\n`);
  return summary;
}
