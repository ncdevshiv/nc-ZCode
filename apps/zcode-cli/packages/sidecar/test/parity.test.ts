/**
 * diff sidecar parity e2e：真实 `zcode-diff` 二进制 + 真实 jsdiff，同输入对输出。
 *
 * fixture（native/crates/zcode-diff/tests/fixtures/jsdiff-parity.json，87 例）由
 * 真实 npm diff@9.0.0 生成；Rust 侧的逐字节 parity 由 crate 测试锁定，本测试锁定
 * **TS 侧 facade 的真实链路**：flag 开 → NDJSON stdio → Rust bin → 回传结构，
 * 与同参数 jsdiff 本地结果逐字段相等。fixture 缺失/二进制缺失 = 测试失败，不跳过。
 *
 * 运行：cd apps/zcode-cli/packages/sidecar && node --import tsx --test test/parity.test.ts
 */
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { after, test } from "node:test";
import { createTwoFilesPatch, diffLines, structuredPatch } from "diff";
import type { StructuredPatch } from "diff";

// flag 必须在调用 facade 前生效（facade 每次调用读 env，这里进程级设置一次）。
process.env.ZCODE_NATIVE_SIDECARS = "diff";

const {
  nativeApplyPatch,
  nativeDiffLines,
  nativeStructuredPatch,
  nativeUnifiedPatch,
  resolveSidecarBin,
  shutdownSidecars,
  sidecarEnabled,
} = await import("../src/index.js");

test("flag semantics: default-on islands, explicit off, kill switch", async () => {
  // 显式 flag（parity 用）开启
  assert.equal(sidecarEnabled("diff"), true);
  const { sidecarEnabled: freshEnabled } = await import(
    `../src/config.js?fresh=${Date.now()}`
  );
  // 默认集合：diff/image/scan/watch 开，proc（等 soak）关
  delete process.env.ZCODE_NATIVE_SIDECARS;
  assert.equal(freshEnabled("diff"), true, "diff is default-on after the cutover flip");
  assert.equal(freshEnabled("image"), true, "image is default-on per the user decision");
  assert.equal(freshEnabled("scan"), true, "scan is default-on after the divergence audit");
  assert.equal(freshEnabled("watch"), true, "watch is default-on after the divergence audit");
  assert.equal(freshEnabled("proc"), false, "proc stays off until MCP stdio soak");
  // kill switch 覆盖一切
  process.env.ZCODE_NATIVE_SIDECARS_OFF = "1";
  const { sidecarEnabled: killedEnabled } = await import(
    `../src/config.js?killed=${Date.now()}`
  );
  assert.equal(killedEnabled("diff"), false, "kill switch must force the TS path");
  assert.equal(killedEnabled("scan"), false, "kill switch must force the TS path");
  delete process.env.ZCODE_NATIVE_SIDECARS_OFF;
  // 复原 parity 进程状态
  process.env.ZCODE_NATIVE_SIDECARS = "diff";
});

// 进程退出前收尾 sidecar；否则回收/退出与在途写竞争，node:test 报
// "asynchronous activity after the test ended"（EPIPE）。
after(async () => {
  await shutdownSidecars();
});

const fixturePath = new URL(
  "../../../../../native/crates/zcode-diff/tests/fixtures/jsdiff-parity.json",
  import.meta.url,
);
const fixtures = JSON.parse(readFileSync(fixturePath, "utf8")) as {
  unified: { name: string; params: Record<string, unknown>; expected: string }[];
  structured: {
    name: string;
    params: Record<string, unknown>;
    expected: Record<string, unknown>;
  }[];
  chunks: { name: string; params: Record<string, unknown>; expected: unknown }[];
  apply: { name: string; params: Record<string, unknown>; expected: unknown }[];
};

const str = (value: unknown): string => String(value);

test("sidecar binary resolves", () => {
  const bin = resolveSidecarBin("zcode-diff");
  assert.ok(bin, "zcode-diff binary must resolve (build native/target or set ZCODE_SIDECAR_DIR)");
});

for (const item of fixtures.unified) {
  test(`diff.unified parity: ${item.name}`, async () => {
    const params = item.params as {
      old: string;
      new: string;
      oldLabel?: string;
      newLabel?: string;
      oldHeader?: string;
      newHeader?: string;
      context?: number;
    };
    const viaSidecar = await nativeUnifiedPatch(
      params.oldLabel as string,
      params.newLabel as string,
      params.old,
      params.new,
      params.oldHeader,
      params.newHeader,
      { context: params.context },
    );
    const viaJsdiff = createTwoFilesPatch(
      params.oldLabel,
      params.newLabel,
      params.old,
      params.new,
      params.oldHeader,
      params.newHeader,
      { context: params.context },
    );
    assert.equal(viaSidecar, viaJsdiff);
    assert.equal(viaSidecar, item.expected, "jsdiff 本地结果必须先与 fixture 一致");
  });
}

for (const item of fixtures.structured) {
  test(`structuredPatch parity: ${item.name}`, async () => {
    const params = item.params as {
      old: string;
      new: string;
      oldLabel?: string;
      newLabel?: string;
      oldHeader?: string;
      newHeader?: string;
      context?: number;
    };
    const viaSidecar = await nativeStructuredPatch(
      params.oldLabel as string,
      params.newLabel as string,
      params.old,
      params.new,
      params.oldHeader,
      params.newHeader,
      { context: params.context },
    );
    const viaJsdiff = structuredPatch(
      params.oldLabel,
      params.newLabel,
      params.old,
      params.new,
      params.oldHeader,
      params.newHeader,
      { context: params.context },
    );
    assert.deepEqual(
      JSON.parse(JSON.stringify(viaSidecar ?? null)),
      JSON.parse(JSON.stringify(viaJsdiff ?? null)),
    );
    assert.deepEqual(
      JSON.parse(JSON.stringify(viaJsdiff ?? null)),
      item.expected,
      "jsdiff 本地结果必须先与 fixture 一致",
    );
  });
}

for (const item of fixtures.chunks) {
  test(`diff.lines parity: ${item.name}`, async () => {
    const params = item.params as { old: string; new: string };
    const viaSidecar = await nativeDiffLines(params.old, params.new);
    const viaJsdiff = diffLines(params.old, params.new);
    assert.deepEqual(viaSidecar, viaJsdiff);
    assert.deepEqual(viaJsdiff, item.expected, "jsdiff 本地结果必须先与 fixture 一致");
  });
}

for (const item of fixtures.apply) {
  test(`diff.applyPatch parity: ${item.name}`, async () => {
    const params = item.params as { source: string; hunks: unknown; fuzzFactor?: number };
    const viaSidecar = await nativeApplyPatch(params.source, params.hunks as StructuredPatch, {
      fuzzFactor: params.fuzzFactor ?? 0,
      autoConvertLineEndings: false,
    });
    const expected = item.expected as { applied: boolean; result?: string };
    if (!expected.applied) {
      assert.equal(viaSidecar, false);
      return;
    }
    assert.equal(viaSidecar, expected.result);
  });
}
