// 语言边界棘轮的可执行规格（specs/typescript-frontend-only.md §3/§4/§5）。
// 每条测试对应 spec 里的一条判定，注释写清"这条防的是哪种绕过"。
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { promises as fs } from "node:fs";
import os from "node:os";
import path from "node:path";
import test from "node:test";

import {
  checkArchitecture,
  checkLanguageBoundary,
  loadPolicy,
  updateTsBaseline,
} from "./index.mjs";
import { classifyPath, matchPattern } from "./policy.mjs";

const POLICY = `version: 1
modules: []
global:
  managedOnly: false
languageBoundary:
  id: ts-frontend-only
  extensions: [.ts, .tsx, .mts, .cts]
  allowlist:
    - id: frontend-webview
      kind: frontend
      roots: [packages/ui, packages/desktop/src/renderer]
    - id: dev-tooling
      kind: dev-tooling
      roots: [scripts, packages/desktop/scripts]
    - id: dev-tooling-config
      kind: dev-tooling
      patterns: ["**/*.config.ts", "**/*.d.ts"]
  forbidDevToolingImports: true
  escapeHatches: []
`;

const HATCH_WITH = (body) => POLICY.replace("  escapeHatches: []", "  escapeHatches:\n" + body);

// 建一个真实 git 仓库：棘轮用 git ls-files 枚举文件集，没有 git 就没有判定。
async function makeRepo({ policy = POLICY } = {}) {
  const dir = await fs.mkdtemp(path.join(os.tmpdir(), "ts-boundary-"));
  execFileSync("git", ["init", "--quiet"], { cwd: dir });
  await fs.writeFile(path.join(dir, "architecture-policy.yaml"), policy);
  return dir;
}

async function write(repo, relative, content) {
  const target = path.join(repo, relative);
  await fs.mkdir(path.dirname(target), { recursive: true });
  await fs.writeFile(target, content);
}

async function boundary(repo) {
  return checkLanguageBoundary({ cwd: repo, policy: await loadPolicy(repo) });
}

const rules = (result) => result.violations.map((item) => item.rule);

test("glob 匹配：** 跨层且可匹配零层，* 只在单层内", () => {
  assert.equal(matchPattern("**/*.d.ts", "a.d.ts"), true);
  assert.equal(matchPattern("**/*.d.ts", "packages/ui/a.d.ts"), true);
  assert.equal(matchPattern("**/*.config.ts", "packages/web/vite.config.ts"), true);
  assert.equal(matchPattern("**/*.config.ts", "packages/web/vite.config.ts.ts"), false);
  assert.equal(matchPattern("packages/*/x.ts", "packages/ui/x.ts"), true);
  assert.equal(matchPattern("packages/*/x.ts", "packages/ui/deep/x.ts"), false);
});

test("新增非前端 TS 违例，allowlist 内的 TS 不违例", async () => {
  const repo = await makeRepo();
  await updateTsBaseline({ cwd: repo });
  await write(repo, "packages/services/src/a.ts", "export const a = 1;\n");
  await write(repo, "packages/ui/src/ok.ts", "export const ok = 1;\n");
  await write(repo, "scripts/build.ts", "export const b = 1;\n");
  await write(repo, "packages/web/vite.config.ts", "export default {};\n");
  const result = await boundary(repo);
  assert.deepEqual(rules(result), ["ts-frontend-only"]);
  assert.equal(result.violations[0].file.endsWith("packages/services/src/a.ts"), true);
  assert.equal(result.stats.files, 1);
  // 违例必须说清怎么修，而不是只说"不允许"
  assert.match(result.violations[0].message, /escapeHatches/);
});

test("扩大违例、缩小与持平合法", async () => {
  const repo = await makeRepo();
  await write(repo, "packages/services/src/a.ts", "export const a = 1;\n");
  await updateTsBaseline({ cwd: repo });
  assert.deepEqual(rules(await boundary(repo)), []);

  await write(repo, "packages/services/src/a.ts", "export const a = 1;\n// more\n");
  assert.deepEqual(rules(await boundary(repo)), ["ts-frontend-only"]);

  await write(repo, "packages/services/src/a.ts", "export const a = 1;\n");
  assert.deepEqual(rules(await boundary(repo)), []);

  const summary = await updateTsBaseline({ cwd: repo });
  assert.equal(summary.delta, 0);
});

test("删除即减债：基线里的陈旧条目不产生违例", async () => {
  const repo = await makeRepo();
  await write(repo, "packages/services/src/a.ts", "export const a = 1;\n");
  await write(repo, "packages/services/src/b.ts", "export const b = 1;\n");
  await updateTsBaseline({ cwd: repo });
  await fs.rm(path.join(repo, "packages/services/src/a.ts"));
  const result = await boundary(repo);
  assert.deepEqual(rules(result), []);
  const summary = await updateTsBaseline({ cwd: repo });
  assert.equal(summary.removed, 1);
  assert.ok(summary.delta < 0);
});

test("在非 allowlist 根内移动文件算新增（旧债不会被搬家洗掉）", async () => {
  const repo = await makeRepo();
  await write(repo, "packages/services/src/a.ts", "export const a = 1;\n");
  await updateTsBaseline({ cwd: repo });
  await fs.mkdir(path.join(repo, "packages/services/src/nested"), { recursive: true });
  await fs.rename(
    path.join(repo, "packages/services/src/a.ts"),
    path.join(repo, "packages/services/src/nested/a.ts"),
  );
  const moved = await boundary(repo);
  assert.deepEqual(rules(moved), ["ts-frontend-only"]);

  // 移进 allowlist 则是合法的前端化，不是债
  const other = await makeRepo();
  await write(other, "packages/services/src/a.ts", "export const a = 1;\n");
  await updateTsBaseline({ cwd: other });
  await fs.rm(path.join(other, "packages/services"), { recursive: true, force: true });
  await write(other, "packages/ui/src/moved.ts", "export const a = 1;\n");
  assert.deepEqual(rules(await boundary(other)), []);
});

test("基线只减不增：无理由扩大被拒，有批准才写入并留痕", async () => {
  const repo = await makeRepo();
  await write(repo, "packages/services/src/a.ts", "export const a = 1;\n");
  await updateTsBaseline({ cwd: repo });
  await write(repo, "packages/services/src/b.ts", "export const b = 1;\n");
  await assert.rejects(() => updateTsBaseline({ cwd: repo }), /拒绝扩大/);
  const summary = await updateTsBaseline({
    cwd: repo,
    acceptGrowth: { reason: "owner 批准的一次性记录", approvedBy: "main" },
  });
  assert.equal(summary.delta > 0, true);
  const written = JSON.parse(
    await fs.readFile(path.join(repo, ".architecture-ts-baseline.json"), "utf8"),
  );
  assert.equal(written.acceptedGrowth.length, 1);
  assert.equal(written.acceptedGrowth[0].approvedBy, "main");
  assert.deepEqual(rules(await boundary(repo)), []);
});

test("逃生舱：合法化非前端 TS", async () => {
  const repo = await makeRepo({
    policy: HATCH_WITH(
      "    - id: frozen-fixture-test\n" +
        '      paths: ["packages/cli/test/*.test.ts"]\n' +
        "      reason: 与冻结 fixture 对比的 parity 测试（S6 明确允许）\n" +
        "      approvedBy: owner\n" +
        "      expires: 2999-01-01\n",
    ),
  });
  await write(repo, "packages/cli/test/parity.test.ts", "test('x', () => {});\n");
  await updateTsBaseline({ cwd: repo });
  const result = await boundary(repo);
  assert.deepEqual(rules(result), []);
  assert.equal(result.stats.files, 0);
});

test("逃生舱缺 reason/approvedBy/expires 会在加载 policy 时被拒", async () => {
  const full = HATCH_WITH(
    "    - id: lazy\n" +
      '      paths: ["x.ts"]\n' +
      "      reason: ok\n" +
      "      approvedBy: owner\n" +
      "      expires: 2999-01-01\n",
  );
  for (const field of ["reason", "approvedBy", "expires"]) {
    const broken = full.replace(new RegExp("^ {6}" + field + ":.*$", "m"), "");
    const dir = await makeRepo({ policy: broken });
    await assert.rejects(
      () => loadPolicy(dir),
      new RegExp(field),
      "缺 " + field + " 的逃生舱应被拒绝",
    );
  }
});

// M1：过期的判定权从"赢下优先级才报"改成"无条件报 expired-exception"。
// 旧实现把到期塞进 language-boundary-ambiguous（"策略有歧义"），于是最该被看见的
// 情况——一条比 allowlist 更宽的过期逃生舱——反而报不出"到期"这件事。
test("过期逃生舱：命中即报 expired-exception，与谁赢下优先级无关", async () => {
  const repo = await makeRepo({
    policy: HATCH_WITH(
      "    - id: stale\n" +
        '      paths: ["packages/ui/src/ok.ts"]\n' +
        "      reason: 曾经合法\n" +
        "      approvedBy: owner\n" +
        "      expires: 2000-01-01\n",
    ),
  });
  await write(repo, "packages/ui/src/ok.ts", "export const ok = 1;\n");
  await updateTsBaseline({ cwd: repo });
  const result = await boundary(repo);
  assert.deepEqual(rules(result), ["expired-exception"]);
  assert.equal(result.violations[0].detail, "stale");
});

// M1 缺口 1：只有一条过期逃生舱、**没有任何 allowlist 命中**时也必须报。
// 旧实现在这里报 "language-boundary-ambiguous"，等于把问题指成"策略有歧义"，
// 而"把 allowlist 放宽"恰恰会让它变得更看不见。
test("过期逃生舱：孤零零一条（无 allowlist 命中）同样报 expired-exception", async () => {
  const repo = await makeRepo({
    policy: HATCH_WITH(
      "    - id: lone-stale\n" +
        '      paths: ["packages/services/legacy/*.ts"]\n' +
        "      reason: 曾经合法\n" +
        "      approvedBy: owner\n" +
        "      expires: 2000-01-01\n",
    ),
  });
  await write(repo, "packages/services/legacy/a.ts", "export const a = 1;\n");
  await updateTsBaseline({ cwd: repo });
  const result = await boundary(repo);
  assert.deepEqual(rules(result), ["expired-exception"]);
  assert.equal(result.violations[0].detail, "lone-stale");
});

// M1 缺口 2：过期逃生舱比 allowlist **更宽**（`1` 个字面量 vs `packages/ui` 的 11）
// 时它会输给优先级，但到期必须照样可见——旧实现在这里报 0 条违例。
test("过期逃生舱：比 allowlist 更宽时（优先级输掉）也必须报到期", async () => {
  const repo = await makeRepo({
    policy: HATCH_WITH(
      "    - id: wide-stale\n" +
        '      paths: ["**/*.ts"]\n' +
        "      reason: 曾经合法\n" +
        "      approvedBy: owner\n" +
        "      expires: 2000-01-01\n",
    ),
  });
  await write(repo, "packages/ui/src/ok.ts", "export const ok = 1;\n");
  await updateTsBaseline({ cwd: repo });
  const result = await boundary(repo);
  assert.deepEqual(rules(result), ["expired-exception"]);
  assert.equal(result.violations[0].detail, "wide-stale");
});

// M1 的另一半：过期不等于更宽松。把一个**非前端**文件从 10 行涨到 5010 行，
// 在一条覆盖它的过期逃生舱下必须照报 ts-frontend-only。
test("过期逃生舱：不能把棘轮放宽（涨行数照报）", async () => {
  const repo = await makeRepo({
    policy: HATCH_WITH(
      "    - id: wide-stale\n" +
        '      paths: ["**/*.ts"]\n' +
        "      reason: 曾经合法\n" +
        "      approvedBy: owner\n" +
        "      expires: 2000-01-01\n",
    ),
  });
  await write(repo, "packages/services/src/big.ts", "export const a = 1;\n");
  await updateTsBaseline({ cwd: repo });
  await write(repo, "packages/services/src/big.ts", "export const a = 1;\n".repeat(5010));
  const result = await boundary(repo);
  assert.ok(rules(result).includes("ts-frontend-only"), "过期逃生舱不得让棘轮变松");
});

test("dev-tooling 停车位：非前端 TS 依赖开发工具代码被拦下，且给出 file:line", async () => {
  const repo = await makeRepo();
  await write(repo, "packages/services/src/a.ts", "export const a = 1;\n");
  await write(repo, "scripts/build.ts", "export const b = 1;\n");
  await updateTsBaseline({ cwd: repo });
  await write(
    repo,
    "packages/services/src/a.ts",
    "import { b } from '../../../scripts/build';\nexport const a = b;\n",
  );
  const result = await boundary(repo);
  assert.ok(rules(result).includes("dev-tooling-import"));
  const hit = result.violations.find((item) => item.rule === "dev-tooling-import");
  assert.equal(hit.detail, "packages/services/src/a.ts:1 -> scripts/build.ts");

  // .mjs 开发工具同样不能被绕过：已知文件集只含 TS 时解析不到 target.mjs（真实漏洞）
  const mjs = await makeRepo();
  await write(mjs, "packages/services/src/a.ts", "export const a = 1;\n");
  await write(mjs, "scripts/tool.mjs", "export const b = 1;\n");
  await updateTsBaseline({ cwd: mjs });
  await write(
    mjs,
    "packages/services/src/a.ts",
    "import { b } from '../../../scripts/tool.mjs';\nexport const a = b;\n",
  );
  assert.ok(rules(await boundary(mjs)).includes("dev-tooling-import"));

  // 反向依赖（开发工具依赖业务）是正常方向，不报
  const ok = await makeRepo();
  await write(ok, "packages/services/src/a.ts", "export const a = 1;\n");
  await write(
    ok,
    "scripts/tool.ts",
    "import { a } from '../packages/services/src/a';\nexport const b = a;\n",
  );
  await updateTsBaseline({ cwd: ok });
  assert.deepEqual(rules(await boundary(ok)), []);
});

// dev-tooling 依赖边同样是棘轮（spec 3.4.1）：既存边是继承债，新增边/同边超额才报。
test("dev-tooling 边是棘轮：新增边与同边超额都报，回到基线就减债", async () => {
  const repo = await makeRepo();
  await write(
    repo,
    "packages/services/src/a.ts",
    "import { b } from '../../../scripts/build';\nexport const a = b;\n",
  );
  await write(repo, "scripts/build.ts", "export const b = 1;\n");
  await write(repo, "scripts/other.ts", "export const c = 1;\n");
  await updateTsBaseline({ cwd: repo });
  const inherited = await boundary(repo);
  assert.deepEqual(rules(inherited), [], "基线生成后既存边不再是违例");
  assert.equal(inherited.stats.edges, 1);

  // 新增一条指向别的工具文件的边 -> 违例
  await write(
    repo,
    "packages/services/src/a.ts",
    "import { b } from '../../../scripts/build';\nimport { c } from '../../../scripts/other';\nexport const a = b + c;\n",
  );
  const addedEdge = await boundary(repo);
  assert.deepEqual(rules(addedEdge), ["ts-frontend-only", "dev-tooling-import"]);
  const hit = addedEdge.violations.find((item) => item.rule === "dev-tooling-import");
  assert.equal(hit.detail, "packages/services/src/a.ts:2 -> scripts/other.ts");
  assert.match(hit.message, /新增依赖/);

  // 同一条边重复导入（条数 2 > 基线 1）-> 违例，且指向第二条导入的 file:line
  await write(
    repo,
    "packages/services/src/a.ts",
    "import { b } from '../../../scripts/build';\nimport { b as b2 } from '../../../scripts/build';\nexport const a = b + b2;\n",
  );
  const grown = await boundary(repo);
  const grownHit = grown.violations.find((item) => item.rule === "dev-tooling-import");
  assert.equal(grownHit.detail, "packages/services/src/a.ts:2 -> scripts/build.ts");
  assert.match(grownHit.message, /扩大依赖/);

  // 回到基线状态 -> 干净；再生成基线时边数不增
  await write(
    repo,
    "packages/services/src/a.ts",
    "import { b } from '../../../scripts/build';\nexport const a = b;\n",
  );
  assert.deepEqual(rules(await boundary(repo)), []);
  const summary = await updateTsBaseline({ cwd: repo });
  assert.equal(summary.edgeDelta, 0);
  assert.deepEqual(summary.grownAxes, []);
});

test("边的扩大同样受「只减不增」约束，并把 axes 记进 acceptedGrowth", async () => {
  const repo = await makeRepo();
  await write(repo, "packages/services/src/a.ts", "export const a = 1;\n");
  await write(repo, "scripts/build.ts", "export const b = 1;\n");
  await updateTsBaseline({ cwd: repo });
  await write(
    repo,
    "packages/services/src/b.ts",
    "import { b } from '../../../scripts/build';\nexport const x = b;\n",
  );
  await assert.rejects(() => updateTsBaseline({ cwd: repo }), /dev-tooling 依赖边/);
  const summary = await updateTsBaseline({
    cwd: repo,
    acceptGrowth: { reason: "owner 批准", approvedBy: "main" },
  });
  // 新增文件本身推高行数，新增边推高边数——两条轴都必须被记下来。
  assert.deepEqual(summary.grownAxes, ["lines", "devToolingEdges"]);
  const written = JSON.parse(
    await fs.readFile(path.join(repo, ".architecture-ts-baseline.json"), "utf8"),
  );
  assert.deepEqual(written.acceptedGrowth[0].axes, ["lines", "devToolingEdges"]);
  assert.equal(written.acceptedGrowth[0].edgeDelta, 1);
  assert.equal(written.acceptedGrowth[0].newEdges, 1);
  assert.deepEqual(rules(await boundary(repo)), []);
});

test("缺少基线文件时硬失败，不静默降级为跳过", async () => {
  const repo = await makeRepo();
  await write(repo, "packages/services/src/a.ts", "export const a = 1;\n");
  await assert.rejects(() => boundary(repo), /architecture-ts-baseline\.json/);
});

// 防"ratchet 只在 --changed 下生效"这个绕过：verify:pre-push 传的就是 --changed。
// changedFiles 为空数组是最强形态——什么都没改，门禁也必须照样看见非前端 TS。
test("ratchet 不受 --changed 影响：改动集为空也照样报", async () => {
  const repo = await makeRepo();
  await write(repo, "packages/services/src/a.ts", "export const a = 1;\n");
  await updateTsBaseline({ cwd: repo });
  await write(repo, "packages/services/src/b.ts", "export const b = 1;\n");
  const scoped = await checkArchitecture({ cwd: repo, changedFiles: [] });
  assert.equal(scoped.newViolations.length, 0, "模块规则按改动集收窄");
  assert.deepEqual(rules(scoped.languageBoundary), ["ts-frontend-only"]);
});

test("allowlist 的 config 模式覆盖 .d.mts / .mts / .cts", async () => {
  const repo = await makeRepo();
  await write(repo, "packages/desktop/scripts/target-platform.d.mts", "export const a: number;\n");
  await write(repo, "packages/x/mod.mts", "export const a = 1;\n");
  await write(repo, "packages/x/mod.cts", "export const a = 1;\n");
  // 基线内的文件不报（棘轮语义）。要证明 .mts/.cts **被判定**，必须在基线之后新增。
  await updateTsBaseline({ cwd: repo });
  await write(repo, "packages/x/legacy.cts", "export const a = 1;\n");
  await write(repo, "packages/x/legacy.mts", "export const a = 1;\n");
  const result = await boundary(repo);
  assert.deepEqual(rules(result), ["ts-frontend-only", "ts-frontend-only"]);
  assert.equal(result.debt.has("packages/x/legacy.cts"), true, ".cts 也要判定为债");
  assert.equal(result.debt.has("packages/x/legacy.mts"), true, ".mts 也要判定为债");
});

// N1：过期逃生舱按**舱**去重。
// reviewer 证明旧代码对一个命中 13 个文件的 `**` 逃生舱吐出 13 条逐字相同的行；
// 真实的宽逃生舱会命中几千个，把唯一可执行的那一行埋掉。下面用 3 个文件钉住
// 「每个舱恰好一条」——单文件 fixture 测不出这个（reviewer 已确认旧 4 条 M1 用例全是单文件）。
test("N1：过期逃生舱命中多个文件时只报一次", async () => {
  const repo = await makeRepo({
    policy: HATCH_WITH(
      "    - id: wide-stale\n" +
        '      paths: ["**/*.ts"]\n' +
        "      reason: 曾经合法\n" +
        "      approvedBy: owner\n" +
        "      expires: 2000-01-01\n",
    ),
  });
  await write(repo, "packages/services/src/a.ts", "export const a = 1;\n");
  await write(repo, "packages/services/src/b.ts", "export const b = 1;\n");
  await write(repo, "packages/rpc/src/c.ts", "export const c = 1;\n");
  await updateTsBaseline({ cwd: repo });
  const result = await boundary(repo);
  const expired = result.violations.filter((item) => item.rule === "expired-exception");
  assert.equal(expired.length, 1, "三个文件命中同一个过期舱，只能报一条");
  assert.equal(expired[0].detail, "wide-stale");
});

// MINOR 2：hatch 的无通配符路径按**精确文件**匹配，所以目录必须在加载时被拒，
// 否则作者以为豁免了整个目录，实际只豁免了一个同名文件。
test("MINOR2：hatch 的 paths 指向目录时拒绝加载并给出 glob 建议", async () => {
  const repo = await makeRepo();
  await write(repo, "packages/services/legacy/a.ts", "export const a = 1;\n");
  const policy = HATCH_WITH(
    "    - id: dir-hatch\n" +
      '      paths: ["packages/services/legacy"]\n' +
      "      reason: 曾经合法\n" +
      "      approvedBy: owner\n" +
      "      expires: 2999-01-01\n",
  );
  await fs.writeFile(path.join(repo, "architecture-policy.yaml"), policy);
  await assert.rejects(() => loadPolicy(repo), /是一个目录/, "目录型 paths 必须报错");
});

// MINOR 1：同字面前缀的平局，只有在**真的含过期条目**时才报（spec 2.2 rule 4），
// 且诊断必须指名那条冲突的逃生舱。未过期的平局不该被贴上「已过期」。
test("MINOR1：活逃生舱同优先级不报过期，过期时必须报并指名冲突舱", async () => {
  const live = await makeRepo({
    policy: HATCH_WITH(
      "    - id: live-hatch\n" +
        '      paths: ["scripts/**"]\n' +
        "      reason: 仍然合法\n" +
        "      approvedBy: owner\n" +
        "      expires: 2999-01-01\n",
    ),
  });
  await write(live, "scripts/legacy/a.ts", "export const a = 1;\n");
  await updateTsBaseline({ cwd: live });
  assert.deepEqual(rules(await boundary(live)), [], "未过期就不该报 expired-exception");

  const stale = await makeRepo({
    policy: HATCH_WITH(
      "    - id: stale-hatch\n" +
        '      paths: ["scripts/**"]\n' +
        "      reason: 曾经合法\n" +
        "      approvedBy: owner\n" +
        "      expires: 2000-01-01\n",
    ),
  });
  await write(stale, "scripts/legacy/a.ts", "export const a = 1;\n");
  await updateTsBaseline({ cwd: stale });
  // 平局且其中含过期条目 -> spec 2.2 rule 4 要求两条都报：到期（事实）与歧义（策略缺陷）。
  assert.deepEqual(rules(await boundary(stale)), [
    "expired-exception",
    "language-boundary-ambiguous",
  ]);
});

// MAJOR 2：活逃生舱放行的 TS 不进 `debt`，所以只报债务会让这个洞看不见
// （审计实测：一条活舱 + 涨 8700 行，标题仍是 OK）。放行量必须单独可见。
test("MAJOR2：被活逃生舱放行的文件出现在 hatched 总量里（含行数）", async () => {
  const repo = await makeRepo({
    policy: HATCH_WITH(
      "    - id: live-hatch\n" +
        '      paths: ["packages/services/src/*.ts"]\n' +
        "      reason: 仍然合法\n" +
        "      approvedBy: owner\n" +
        "      expires: 2999-01-01\n",
    ),
  });
  await write(repo, "packages/services/src/a.ts", "export const a = 1;\n");
  await write(repo, "packages/services/src/b.ts", "export const b = 1;\n");
  await updateTsBaseline({ cwd: repo });
  const before = await boundary(repo);
  assert.equal(before.stats.hatchedFiles, 2, "两个文件被活舱放行");
  assert.equal(before.stats.hatchedLines, 4, "放行量按行数报，口径与债务侧一致");

  // 放行之后可以无限增长——本轮不堵（那是设计决策），但增长必须**看得见**。
  await write(repo, "packages/services/src/a.ts", "export const a = 1;\n".repeat(900));
  const after = await boundary(repo);
  assert.equal(after.stats.hatchedFiles, 2);
  // a.ts = 900 行正文 + 尾换行 = 901（split 会多出最后一段），b.ts = 2。
  assert.equal(after.stats.hatchedLines, 903, "放行量的增长必须体现在上报数字里");
  assert.ok(after.stats.lines < after.stats.hatchedLines, "债务总量不含放行量——所以两者必须并排报");
});

test("优先级：字面前缀最长者胜，hatch 压不过更具体的 allowlist", () => {
  const policy = {
    extensions: [".ts"],
    allowlist: [
      { id: "wide", kind: "frontend", items: [{ type: "root", value: "packages", literal: 8 }] },
      {
        id: "narrow",
        kind: "dev-tooling",
        items: [{ type: "root", value: "packages/ui", literal: 11 }],
      },
    ],
    forbidDevToolingImports: true,
    escapeHatches: [
      {
        id: "h",
        reason: "r",
        approvedBy: "o",
        expires: "2999-01-01",
        items: [{ type: "file", value: "packages/ui/src/a.ts", literal: 18 }],
      },
    ],
  };
  // hatch（18）压过 allowlist（11）—— 逃生舱是显式授权，优先于更宽的默认允许
  assert.equal(classifyPath("packages/ui/src/a.ts", policy, "2026-01-01").verdict, "hatched");
  // 更具体的 allowlist 压过更宽的 allowlist
  assert.equal(
    classifyPath("packages/ui/src/other.ts", policy, "2026-01-01").allowlist.id,
    "narrow",
  );
  // 两个 allowlist 都不命中 = 迁移债
  assert.equal(classifyPath("apps/cli/a.ts", policy, "2026-01-01").verdict, "debt");
});
