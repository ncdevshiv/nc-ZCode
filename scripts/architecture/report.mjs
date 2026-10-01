// 报告渲染：把两类规则的结果写成终端文本与 Markdown。
//
// 单独成文件是因为这是**呈现层**：它不该和规则引擎（模块规则在 index.mjs、语言边界
// 棘轮在 language-boundary.mjs）住在同一个文件里，否则每加一条规则就要动一次渲染。

export function formatReport(result) {
  const lines = [
    `architecture: ${result.newViolations.length === 0 && !result.languageBoundary?.violations.length ? "OK" : "FAILED"}`,
    `violations: ${result.violations.length}`,
    `baseline: ${result.baselineViolations.length}`,
    `new: ${result.newViolations.length}`,
  ];
  for (const item of result.newViolations)
    lines.push(`- ${item.rule} ${item.file}: ${item.message}`);
  const boundary = result.languageBoundary;
  if (boundary) {
    const { stats, violations } = boundary;
    const status = violations.length === 0 ? "OK" : "FAILED";
    lines.push(
      `ts-boundary(${boundary.rule}): ${status} debt=${stats.files} files / ${stats.lines} lines` +
        ` (baseline ${stats.baselineFiles} files / ${stats.baselineLines} lines)` +
        ` devToolingEdges=${stats.edges} (baseline ${stats.baselineEdges})` +
        // 放行量必须与债务量并排出现在同一行：只看债务量会让人以为某个被活逃生舱
        // 放行的巨大文件「不存在」（审计 MAJOR-2 的整条发现就是这个）。
        ` | hatched=${stats.hatchedFiles} files / ${stats.hatchedLines} lines`,
    );
    for (const item of violations) lines.push(`- ${item.rule} ${item.detail}: ${item.message}`);
  }
  return lines.join("\n");
}

export function formatMarkdownReport(result) {
  const lines = [
    `# Architecture report`,
    "",
    `- Status: **${result.newViolations.length === 0 && !result.languageBoundary?.violations.length ? "OK" : "FAILED"}**`,
    `- Violations: ${result.violations.length}`,
    `- Baseline: ${result.baselineViolations.length}`,
    `- New: ${result.newViolations.length}`,
  ];
  if (result.newViolations.length > 0) {
    lines.push("", "## New violations", "", "| Rule | File | Message |", "| --- | --- | --- |");
    for (const item of result.newViolations)
      lines.push(`| ${item.rule} | ${item.file} | ${item.message.replaceAll("|", "\\|")} |`);
  }
  const boundary = result.languageBoundary;
  if (boundary) {
    const { stats, violations } = boundary;
    lines.push(
      "",
      "## TypeScript language boundary",
      "",
      `- Rule: \`${boundary.rule}\` (specs/typescript-frontend-only.md)`,
      `- Status: **${violations.length === 0 ? "OK" : "FAILED"}**`,
      `- Migration debt: ${stats.files} files / ${stats.lines} lines`,
      `- Baseline: ${stats.baselineFiles} files / ${stats.baselineLines} lines`,
      `- Allowed (frontend + dev tooling): ${stats.allowedFiles} files`,
      `- dev-tooling dependency edges: ${stats.edges} (baseline ${stats.baselineEdges})`,
      `- Hatch-exempt (live escape hatch): ${stats.hatchedFiles} files / ${stats.hatchedLines} lines`,
    );
    if (violations.length > 0) {
      lines.push("", "| Rule | Evidence | Message |", "| --- | --- | --- |");
      for (const item of violations)
        lines.push(`| ${item.rule} | ${item.detail} | ${item.message.replaceAll("|", "\\|")} |`);
    }
  }
  return lines.join("\n");
}
