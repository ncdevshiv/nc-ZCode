#!/usr/bin/env node
import {
  checkArchitecture,
  changedFilesFromGit,
  formatMarkdownReport,
  formatReport,
  generateContext,
  updateBaseline,
  updateTsBaseline,
} from "./index.mjs";

const args = process.argv.slice(2);
const command = args[0] ?? "check";
const cwd = process.cwd();

function option(name) {
  const index = args.indexOf(`--${name}`);
  return index === -1 ? null : (args[index + 1] ?? null);
}

if (command === "context") {
  const moduleId = args[1];
  if (!moduleId) throw new Error("用法: bun run architecture:context <module-id>");
  console.log(await generateContext({ cwd, moduleId }));
  process.exit(0);
}

if (command === "ts-baseline:update") {
  const reason = option("reason");
  const approvedBy = option("approved-by");
  const acceptGrowth = args.includes("--accept-growth") ? { reason, approvedBy } : null;
  // 批准扩大必须三件套齐全，缺一件就当成"没批准"，让基线保持只减不增。
  const accept = reason && approvedBy ? acceptGrowth : null;
  if (acceptGrowth && !accept)
    throw new Error('--accept-growth 需要同时提供 --reason "<为什么>" 与 --approved-by "<谁>"');
  const summary = await updateTsBaseline({ cwd, acceptGrowth: accept });
  console.log(
    `ts-baseline${summary.bootstrapped ? " bootstrapped" : " updated"}: ${summary.previousLines}->${summary.lines} lines` +
      ` (新增 ${summary.added} / 删除 ${summary.removed} / 扩大 ${summary.grown} / 缩小 ${summary.shrunk}, Δ${summary.delta >= 0 ? "+" : ""}${summary.delta})` +
      `; dev-tooling 边 ${summary.previousEdgeCount}->${summary.edgeCount} (新增 ${summary.newEdges} / 扩大 ${summary.grownEdges} / 移除 ${summary.removedEdges}, Δ${summary.edgeDelta >= 0 ? "+" : ""}${summary.edgeDelta})` +
      // 舱放行的量必须和 Δ 并排：舱藏起来的文件在基线里"消失"，Δ 为负时若不报出
      // 舱的覆盖面，这条命令会把隐藏读成减债（审计 MINOR-B）。
      `; 活逃生舱放行 ${summary.hatchedFiles} files / ${summary.hatchedLines} lines`,
  );
  process.exit(0);
}

const changed = args.includes("--changed") ? await changedFilesFromGit(cwd) : null;
const result = await checkArchitecture({ cwd, changedFiles: changed });
const languageViolations = result.languageBoundary?.violations ?? [];

if (command === "baseline:update") {
  await updateBaseline({ cwd, violations: result.violations });
  console.log(
    `baseline updated: ${result.violations.length} violations` +
      `（模块规则基线；语言边界基线请用 bun run architecture:ts-baseline:update）`,
  );
  process.exit(0);
}

const failed = result.newViolations.length > 0 || languageViolations.length > 0;

if (command === "report") {
  console.log(
    args.includes("--markdown") ? formatMarkdownReport(result) : JSON.stringify(result, null, 2),
  );
  process.exit(failed ? 1 : 0);
}

console.log(formatReport(result));
process.exit(failed ? 1 : 0);
