#!/usr/bin/env node
// 用真实 npm diff@9（jsdiff）生成 zcode-diff 的 parity fixtures。
// 运行方式（仓库根目录）：node native/crates/zcode-diff/tools/gen-jsdiff-fixtures.mjs
// 输出：native/crates/zcode-diff/tests/fixtures/jsdiff-parity.json
//
// 解析锚点指向 apps/zcode-cli/packages/core（该包声明 "diff": "^9.0.0"），
// 保证与 TS 运行时使用同一个 jsdiff 版本；版本不符直接失败，避免基线漂移。

import fs from "node:fs";
import path from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const toolsDir = path.dirname(fileURLToPath(import.meta.url));
const crateDir = path.resolve(toolsDir, "..");
const repoRoot = path.resolve(crateDir, "..", "..", "..");
const coreDir = path.join(repoRoot, "apps", "zcode-cli", "packages", "core");
const require = createRequire(path.join(coreDir, "package.json"));
const version = require("diff/package.json").version;
if (!version.startsWith("9.")) {
  throw new Error(`需要 jsdiff 9.x，实际解析到 ${version}`);
}
process.chdir(coreDir);
const { structuredPatch, createTwoFilesPatch, diffLines, applyPatch } = require("diff");

const DEFAULT_CONTEXT = 4;
const DEFAULT_TIMEOUT_MS = 5000;

const unified = [];
const structured = [];
const chunks = [];
const apply = [];

// ---- 构造输入 ----------------------------------------------------------

const lines = (count, prefix = "line") =>
  Array.from({ length: count }, (_, i) => `${prefix} ${i}\n`).join("");

const replaceLine = (text, index, value) => {
  const rows = text.split("\n");
  rows[index] = value;
  return rows.join("\n");
};

// ---- unified / structured / chunks 三类用例 ----------------------------

function addCase(name, input, options = {}) {
  const context = options.context ?? DEFAULT_CONTEXT;
  const timeoutMs = options.timeoutMs ?? DEFAULT_TIMEOUT_MS;
  // jsdiff 选项键是 timeout（毫秒数），与 sidecar 的 timeoutMs 对应。
  const opts = { context, timeout: timeoutMs };
  const { old, new: next } = input;
  const oldLabel = "oldLabel" in input ? input.oldLabel : undefined;
  const newLabel = "newLabel" in input ? input.newLabel : undefined;
  const oldHeader = "oldHeader" in input ? input.oldHeader : undefined;
  const newHeader = "newHeader" in input ? input.newHeader : undefined;

  const patchText = createTwoFilesPatch(oldLabel, newLabel, old, next, undefined, undefined, opts);
  unified.push({
    name,
    params: {
      old,
      new: next,
      ...(oldLabel !== undefined ? { oldLabel } : {}),
      ...(newLabel !== undefined ? { newLabel } : {}),
      context,
      ...(timeoutMs !== DEFAULT_TIMEOUT_MS ? { timeoutMs } : {}),
    },
    expected: patchText === undefined ? null : patchText,
  });

  const sp = structuredPatch(oldLabel, newLabel, old, next, oldHeader, newHeader, opts);
  structured.push({
    name,
    params: {
      old,
      new: next,
      ...(oldLabel !== undefined ? { oldLabel } : {}),
      ...(newLabel !== undefined ? { newLabel } : {}),
      ...(oldHeader !== undefined ? { oldHeader } : {}),
      ...(newHeader !== undefined ? { newHeader } : {}),
      context,
      ...(timeoutMs !== DEFAULT_TIMEOUT_MS ? { timeoutMs } : {}),
    },
    expected: sp === undefined ? null : JSON.parse(JSON.stringify(sp)),
  });

  const parts = diffLines(old, next);
  chunks.push({
    name,
    params: { old, new: next },
    expected: JSON.parse(JSON.stringify(parts)),
  });
}

const thousand = lines(1000);
const farApartOld = lines(40, "n");
// replaceLine 按 \n 切分赋值：替换值本身不得含 \n（换行由 join 统一补回）。
const farApartNew = replaceLine(replaceLine(lines(40, "n"), 0, "zero"), 39, "thirty-nine");

const cases = [
  ["identical", { old: "a\nb\n", new: "a\nb\n", oldLabel: "f.txt", newLabel: "f.txt" }],
  ["insert", { old: "a\n", new: "a\nb\n", oldLabel: "f.txt", newLabel: "f.txt" }],
  ["delete", { old: "a\nb\n", new: "a\n", oldLabel: "f.txt", newLabel: "f.txt" }],
  ["replace", { old: "a\nb\nc\n", new: "a\nB\nc\n", oldLabel: "f.txt", newLabel: "f.txt" }],
  ["no-newline-old", { old: "a\nb", new: "a\nb\n", oldLabel: "f.txt", newLabel: "f.txt" }],
  ["no-newline-new", { old: "a\nb\n", new: "a\nb", oldLabel: "f.txt", newLabel: "f.txt" }],
  ["no-newline-both", { old: "a\nb", new: "a\nB", oldLabel: "f.txt", newLabel: "f.txt" }],
  ["single-line-no-nl", { old: "x", new: "y", oldLabel: "f.txt", newLabel: "f.txt" }],
  [
    "crlf",
    { old: "a\r\nb\r\nc\r\n", new: "a\r\nB\r\nc\r\n", oldLabel: "f.txt", newLabel: "f.txt" },
  ],
  [
    "cjk-content",
    {
      old: "中文 一行\n第二行\n",
      new: "中文 改过\n第二行\n",
      oldLabel: "f.txt",
      newLabel: "f.txt",
    },
  ],
  ["cjk-label", { old: "a\n", new: "b\n", oldLabel: "中文 文件.txt", newLabel: "中文 文件.txt" }],
  ["empty-to-content", { old: "", new: "x\n", oldLabel: "f.txt", newLabel: "f.txt" }],
  ["content-to-empty", { old: "x\n", new: "", oldLabel: "f.txt", newLabel: "f.txt" }],
  ["both-empty", { old: "", new: "", oldLabel: "f.txt", newLabel: "f.txt" }],
  [
    "thousand-lines",
    {
      old: thousand,
      new: replaceLine(thousand, 500, "changed"),
      oldLabel: "f.txt",
      newLabel: "f.txt",
    },
  ],
  ["far-apart", { old: farApartOld, new: farApartNew, oldLabel: "f.txt", newLabel: "f.txt" }],
  [
    "overlap-merge",
    { old: "a\nb\nc\nd\ne\nf\n", new: "A\nb\nc\nd\ne\nF\n", oldLabel: "f.txt", newLabel: "f.txt" },
  ],
  [
    "context-3",
    {
      old: "1\n2\n3\n4\n5\n6\n7\n",
      new: "1\n2\n3\n4\nX\n6\n7\n",
      oldLabel: "f.txt",
      newLabel: "f.txt",
    },
    { context: 3 },
  ],
  ["no-labels", { old: "a\n", new: "b\n" }],
  ["diff-labels", { old: "a\n", new: "b\n", oldLabel: "a.txt", newLabel: "b.txt" }],
  [
    "headers",
    {
      old: "a\n",
      new: "b\n",
      oldLabel: "f.txt",
      newLabel: "f.txt",
      oldHeader: "before",
      newHeader: "after",
    },
  ],
  [
    "header-empty",
    {
      old: "a\n",
      new: "b\n",
      oldLabel: "f.txt",
      newLabel: "f.txt",
      oldHeader: "before",
      newHeader: "",
    },
  ],
  ["quoted-name", { old: "a\n", new: "b\n", oldLabel: 'q"uote\\.txt', newLabel: 'q"uote\\.txt' }],
  ["tab-name", { old: "a\n", new: "b\n", oldLabel: "with\ttab.txt", newLabel: "with\ttab.txt" }],
];
for (const [name, input, options] of cases) {
  addCase(name, input, options);
}

// context=0：无任何上下文行（边界参数）。
addCase(
  "context-0",
  { old: "a\nb\nc\n", new: "a\nX\nc\n", oldLabel: "f.txt", newLabel: "f.txt" },
  { context: 0 },
);

// ---- apply 用例 ---------------------------------------------------------

function addApply(name, patchOld, patchNew, source) {
  const sp = structuredPatch("f", "f", patchOld, patchNew, undefined, undefined, {
    context: DEFAULT_CONTEXT,
  });
  const patchObj = { oldFileName: "f", newFileName: "f", hunks: sp.hunks };
  const result = applyPatch(source, patchObj, {
    autoConvertLineEndings: false,
    fuzzFactor: 0,
  });
  apply.push({
    name,
    params: { source, hunks: JSON.parse(JSON.stringify(sp.hunks)) },
    expected: result === false ? { applied: false } : { applied: true, result },
  });
}

addApply("replace-ok", "a\nb\nc\n", "a\nB\nc\n", "a\nb\nc\n");
addApply("drift-fail", "a\nb\nc\n", "a\nB\nc\n", "z\ny\nq\n");
addApply("drift-position", "a\nb\nc\n", "a\nB\nc\n", "z\na\nb\nc\n");
addApply("no-newline-both", "a\nb", "a\nB", "a\nb");
addApply("empty-hunks", "a\n", "a\n", "a\n");
addApply("insert-start", "b\n", "a\nb\n", "b\n");
addApply("crlf", "a\r\nb\r\n", "a\r\nB\r\n", "a\r\nb\r\n");
addApply("delete-all", "a\nb\n", "", "a\nb\n");
addApply("add-newline", "a", "a\n", "a");
addApply("remove-newline", "a\n", "a", "a\n");
addApply("source-untouched", "a\nb\nc\n", "a\nB\nc\n", "a\nb\nc\nextra\n");
addApply("empty-source", "", "x\n", "");

// ---- 写出 --------------------------------------------------------------

const fixture = { jsdiff: version, unified, structured, chunks, apply };
const outPath = path.join(crateDir, "tests", "fixtures", "jsdiff-parity.json");
fs.mkdirSync(path.dirname(outPath), { recursive: true });
fs.writeFileSync(outPath, `${JSON.stringify(fixture, null, 2)}\n`, "utf8");
console.log(
  `jsdiff@${version} → ${path.relative(process.cwd(), outPath)} ` +
    `(unified ${unified.length}, structured ${structured.length}, ` +
    `chunks ${chunks.length}, apply ${apply.length})`,
);
